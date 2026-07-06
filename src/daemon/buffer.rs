//! BUFFER-001: size enforcement.  BUFFER-002: exponential backoff.
//! BUFFER-003: corruption recovery (skip bad lines).
// This is the safety net for when Cognee is unreachable. Instead of losing events when a
// flush fails, we append them as newline-delimited JSON to a file on disk and keep trying
// to replay that file later. It's basically a tiny durable queue, no database needed, just
// an append-only file plus a simple backoff so we don't hammer a service that's already down.

use crate::cli::Config;
use crate::events::Event;
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use std::io::Write;
use std::time::{Duration, Instant};

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
pub(crate) fn should_retry() -> bool {
    let state = RETRY_STATE.lock().unwrap();
    match state.last_attempt {
        None => true,
        Some(t) => t.elapsed() >= Duration::from_secs(state.backoff_secs),
    }
}

pub(crate) fn record_success() {
    let mut state = RETRY_STATE.lock().unwrap();
    state.backoff_secs = 60;
    state.last_attempt = Some(Instant::now());
}

pub(crate) fn record_failure() {
    let mut state = RETRY_STATE.lock().unwrap();
    state.backoff_secs = (state.backoff_secs * 2).min(MAX_BACKOFF);
    state.last_attempt = Some(Instant::now());
}

pub async fn store_events(events: &[Event]) -> Result<()> {
    let config = Config::load()?;
    let buf_path = &config.offline_buffer_path;

    if let Some(p) = buf_path.parent() {
        std::fs::create_dir_all(p)?;
    }

    // BUFFER-001: enforce size limit before appending
    let existing_count = count_buffer_lines(buf_path);
    if existing_count >= config.max_buffer_size {
        warn!(
            "Buffer at limit ({}). Dropping {} oldest events.",
            config.max_buffer_size,
            events.len()
        );
        trim_buffer(buf_path, events.len())?;
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(buf_path)
        .with_context(|| format!("Cannot open buffer: {:?}", buf_path))?;

    for event in events {
        let json = serde_json::to_string(event)?;
        writeln!(file, "{}", json)?;
    }

    debug!("Buffered {} events", events.len());
    Ok(())
}

pub async fn flush_buffered_events() -> Result<()> {
    if !should_retry() {
        return Ok(());
    }

    let config = Config::load()?;
    let buf_path = &config.offline_buffer_path;
    if !buf_path.exists() {
        return Ok(());
    }

    let content = std::fs::read_to_string(buf_path)?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    if lines.is_empty() {
        return Ok(());
    }

    // BUFFER-005: this used to call remember(vec![event]) once per line, one HTTP
    // round trip per buffered event, each with its own retry ladder. For a buffer
    // built up during any real outage that meant hundreds of serialized requests
    // to replay a few hundred events. remember() already chunks into batches of up
    // to 500 (see ingest.rs's CHUNK_SIZE) and sends each chunk as one multipart
    // request, so replay the buffer through that same batching instead of
    // re-implementing a slower one-at-a-time path here.
    let mut events = Vec::with_capacity(lines.len());
    let mut corrupt = 0usize;
    for line in &lines {
        // BUFFER-003: skip corrupt lines silently, but keep the rest of the batch.
        match serde_json::from_str::<Event>(line) {
            Ok(e) => events.push(e),
            Err(e) => {
                warn!("Skipping corrupt buffer line: {}", e);
                corrupt += 1;
            }
        }
    }

    if events.is_empty() {
        if corrupt > 0 {
            std::fs::write(buf_path, "")?;
        }
        return Ok(());
    }

    let total = events.len();
    match crate::cognee::remember(events).await {
        Ok(_) => {
            info!("Flushed {} buffered events", total);
            record_success();
            std::fs::write(buf_path, "")?;
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
        // Just verify that corrupt JSON doesn't panic when parsing
        let result = serde_json::from_str::<Event>("this is not json");
        assert!(result.is_err());
    }
}
