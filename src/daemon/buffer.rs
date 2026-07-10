//! BUFFER-001: size enforcement.  BUFFER-002: exponential backoff.
//! BUFFER-003: corruption recovery (skip bad lines).
// This is the safety net for when Cognee is unreachable. Instead of losing events when a
// flush fails, we append them as newline-delimited JSON to a file on disk and keep trying
// to replay that file later. It's basically a tiny durable queue, no database needed, just
// an append-only file plus a simple backoff so we don't hammer a service that's already down.

use crate::{cli::Config, events::Event};
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::{
    io::Write,
    time::{Duration, Instant},
};

// Backoff state stored as a module-level Mutex (single daemon instance).
// I went with a plain static Mutex instead of threading this state through function
// arguments everywhere, since there's only ever one daemon process and the state genuinely
// is global to it. Feels like the honest representation of what this is rather than
// pretending it's more functional than it needs to be.
static RETRY_STATE: std::sync::Mutex<RetryState> = std::sync::Mutex::new(RetryState {
    backoff_secs: 60,
    last_attempt: None,
});

struct RetryState {
    backoff_secs: u64,
    last_attempt: Option<Instant>,
}

// Backoff doubles on every failure (60s, 120s, 240s...) up to this ceiling, so we never
// wait longer than an hour between retry attempts even during a long outage.
const MAX_BACKOFF: u64 = 3600;

// BUFFER-004: this backoff gate used to only wrap flush_buffered_events(). The live
// path (daemon/mod.rs's do_flush(), called every flush_timer tick) had no cooldown
// at all, so during an outage every tick re-ran its own full retry ladder from
// scratch while the buffer replay separately ran its own. Making should_retry() /
// record_success() / record_failure() pub(crate) lets daemon/mod.rs check the same
// gate before attempting a live flush, so both paths share one circuit breaker
// instead of two uncoordinated ones hammering Cognee independently.
//
// Every .lock() here recovers from a poisoned mutex rather than unwrapping straight into a
// panic (see daemon::discovery's rate limiter for the same reasoning in more detail):
// worst case a poisoned lock costs us a slightly-off backoff timer, not a crash, and a
// crash here would take down the whole daemon over what's just retry bookkeeping.
pub(crate) fn should_retry() -> bool {
    let state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match state.last_attempt {
        None => true,
        Some(t) => t.elapsed() >= Duration::from_secs(state.backoff_secs),
    }
}

pub(crate) fn record_success() {
    let mut state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.backoff_secs = 60;
    state.last_attempt = Some(Instant::now());
}

pub(crate) fn record_failure() {
    let mut state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.backoff_secs = (state.backoff_secs * 2).min(MAX_BACKOFF);
    state.last_attempt = Some(Instant::now());
}

pub async fn store_events(events: &[Event]) -> Result<()> {
    let config = Config::load()?;
    let buf_path = config.offline_buffer_path.clone();
    let max_buffer_size = config.max_buffer_size;

    // Everything here (creating the parent dir, counting existing lines, trimming if
    // we're over the limit, opening in append mode, and writing each line) is synchronous
    // std::fs work. Bundled into one spawn_blocking closure rather than each call wrapped
    // separately, since it's really one logical unit of blocking work and there's no reason
    // to pay for multiple trips to tokio's blocking thread pool when one covers it all.
    let serialized: Vec<String> = events
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<_, _>>()?;
    let event_count = events.len();

    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(p) = buf_path.parent() {
            std::fs::create_dir_all(p)?;
        }

        // BUFFER-001: enforce size limit before appending
        let existing_count = count_buffer_lines(&buf_path);
        if existing_count >= max_buffer_size {
            warn!(
                "Buffer at limit ({}). Dropping {} oldest events.",
                max_buffer_size, event_count
            );
            trim_buffer(&buf_path, event_count)?;
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&buf_path)
            .with_context(|| format!("Cannot open buffer: {:?}", buf_path))?;

        for json in &serialized {
            writeln!(file, "{}", json)?;
        }

        debug!("Buffered {} events", event_count);
        Ok(())
    })
    .await
    .context("buffer write task panicked")?
}

pub async fn flush_buffered_events() -> Result<()> {
    if !should_retry() {
        return Ok(());
    }

    let config = Config::load()?;
    let buf_path = config.offline_buffer_path.clone();

    // Reading and parsing the buffer file is synchronous std::fs work, so it's bundled into
    // one spawn_blocking closure rather than left running directly on an async worker thread.
    let read_path = buf_path.clone();
    let (events, corrupt) = tokio::task::spawn_blocking(move || -> Result<(Vec<Event>, usize)> {
        if !read_path.exists() {
            return Ok((Vec::new(), 0));
        }
        let content = std::fs::read_to_string(&read_path)?;
        Ok(parse_buffer_lines(&content))
    })
    .await
    .context("buffer read task panicked")??;

    if events.is_empty() {
        if corrupt > 0 {
            let clear_path = buf_path.clone();
            tokio::task::spawn_blocking(move || std::fs::write(clear_path, ""))
                .await
                .context("buffer clear task panicked")??;
        }
        return Ok(());
    }

    let total = events.len();
    match crate::cognee::remember(events).await {
        Ok(_) => {
            info!("Flushed {} buffered events", total);
            record_success();
            tokio::task::spawn_blocking(move || std::fs::write(buf_path, ""))
                .await
                .context("buffer clear task panicked")??;
        }
        Err(e) => {
            // All-or-nothing: remember() chunks internally, but if any chunk in the
            // batch fails we can't cheaply tell which events made it through
            // without per-item round trips. Leaving the whole buffer in place and
            // retrying the full batch next window is simpler and, given
            // should_retry()'s backoff, doesn't cost extra requests. It's
            // strictly fewer than the old per-line approach even in the worst case.
            error!("Buffer flush failed: {}. Will retry after backoff.", e);
            record_failure();
        }
    }

    Ok(())
}

// Small helper shared by store_events and the size check, counts how many non-blank lines
// are already sitting in the buffer file so we know whether we're about to blow past the
// configured max_buffer_size.
fn count_buffer_lines(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count())
        .unwrap_or(0)
}

/// Drop the oldest `n` lines from the buffer to make room.
fn trim_buffer(path: &std::path::Path, drop_n: usize) -> Result<()> {
    let content = std::fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.len() <= drop_n {
        std::fs::write(path, "")?;
        return Ok(());
    }
    let kept = &lines[drop_n..];
    std::fs::write(path, kept.join("\n") + "\n")?;
    Ok(())
}

/// BUFFER-003: parses NDJSON buffer content into (valid events, corrupt line count),
/// skipping any line that doesn't parse rather than failing the whole batch over one bad
/// line. Pulled out as its own pure function (no I/O, no async, just a &str in and a result
/// out) specifically so this exact behavior, skip the bad ones, keep the rest, can be
/// tested directly against a real mixed batch, both here and from tests/integration.rs,
/// rather than only ever being exercised indirectly through flush_buffered_events's
/// spawn_blocking closure.
pub fn parse_buffer_lines(content: &str) -> (Vec<Event>, usize) {
    let mut events = Vec::new();
    let mut corrupt = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<Event>(line) {
            Ok(e) => events.push(e),
            Err(e) => {
                warn!("Skipping corrupt buffer line: {}", e);
                corrupt += 1;
            }
        }
    }
    (events, corrupt)
}

// ── Unit tests (TEST-005) ────
#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, ShellCommandEvent};
    use chrono::Utc;

    fn sample_event() -> Event {
        Event::ShellCommand(ShellCommandEvent {
            timestamp: Utc::now(),
            directory: "/tmp".into(),
            command: "echo hello".into(),
            exit_code: Some(0),
            output: None,
            duration_ms: None,
            session_id: None,
            command_hash: None,
            error_type: None,
        })
    }

    #[test]
    fn test_count_lines_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("buf.ndjson");
        assert_eq!(count_buffer_lines(&p), 0);
    }

    #[test]
    fn test_trim_buffer() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("buf.ndjson");
        std::fs::write(&p, "line1\nline2\nline3\n").unwrap();
        trim_buffer(&p, 1).unwrap();
        let content = std::fs::read_to_string(&p).unwrap();
        assert!(!content.contains("line1"));
        assert!(content.contains("line2"));
    }

    #[test]
    fn test_corrupt_line_skipped() {
        // A real mixed batch: two valid events with one corrupt line sandwiched between
        // them. This is what actually matters, not just that serde_json errors on garbage
        // (it obviously does), but that our own parse_buffer_lines correctly separates the
        // good from the bad and keeps going instead of losing the whole batch.
        let valid_one = serde_json::to_string(&sample_event()).unwrap();
        let valid_two = serde_json::to_string(&sample_event()).unwrap();
        let content = format!("{}\nthis is not json at all\n{}\n", valid_one, valid_two);

        let (events, corrupt) = parse_buffer_lines(&content);

        assert_eq!(events.len(), 2, "both valid lines should have been parsed");
        assert_eq!(
            corrupt, 1,
            "exactly one corrupt line should have been counted"
        );
    }

    #[test]
    fn test_parse_buffer_lines_all_valid() {
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&sample_event()).unwrap(),
            serde_json::to_string(&sample_event()).unwrap()
        );
        let (events, corrupt) = parse_buffer_lines(&content);
        assert_eq!(events.len(), 2);
        assert_eq!(corrupt, 0);
    }

    #[test]
    fn test_parse_buffer_lines_all_corrupt() {
        let content = "not json\nalso not json\n{broken";
        let (events, corrupt) = parse_buffer_lines(content);
        assert_eq!(events.len(), 0);
        assert_eq!(corrupt, 3);
    }

    #[test]
    fn test_parse_buffer_lines_ignores_blank_lines() {
        let content = format!(
            "\n\n{}\n\n",
            serde_json::to_string(&sample_event()).unwrap()
        );
        let (events, corrupt) = parse_buffer_lines(&content);
        assert_eq!(events.len(), 1);
        assert_eq!(corrupt, 0);
    }
}
