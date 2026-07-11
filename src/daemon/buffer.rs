//! BUFFER-001: size enforcement.  BUFFER-002: exponential backoff.
//! BUFFER-003: corruption recovery (skip bad lines).
//! BUFFER-004: persistent retry state across daemon restarts.
// This is the safety net for when Cognee is unreachable. Instead of losing events when a
// flush fails, we append them as newline-delimited JSON to a file on disk and keep trying
// to replay that file later. It's basically a tiny durable queue, no database needed, just
// an append-only file plus a simple backoff so we don't hammer a service that's already down.

use crate::{cli::Config, events::Event};
use anyhow::{Context, Result};
use log::{debug, error, info, warn};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::PathBuf,
    time::{Duration, Instant},
};

// Backoff state stored persistently on disk so it survives daemon restarts.
// This prevents the daemon from hammering a failing service after a restart.
const RETRY_STATE_FILE: &str = "retry_state.json";

#[derive(Debug, Serialize, Deserialize)]
struct PersistentRetryState {
    backoff_secs: u64,
    last_attempt: Option<chrono::DateTime<chrono::Utc>>,
}

impl Default for PersistentRetryState {
    fn default() -> Self {
        Self {
            backoff_secs: 30, // Start with 30s, not 60s
            last_attempt: None,
        }
    }
}

// Backoff state stored as a module-level Mutex (single daemon instance).
// I went with a plain static Mutex instead of threading this state through function
// arguments everywhere, since there's only ever one daemon process and the state genuinely
// is global to it. Feels like the honest representation of what this is rather than
// pretending it's more functional than it needs to be.
static RETRY_STATE: std::sync::Mutex<RetryState> = std::sync::Mutex::new(RetryState {
    backoff_secs: 30,
    last_attempt: None,
});

struct RetryState {
    backoff_secs: u64,
    last_attempt: Option<Instant>,
}

impl Default for RetryState {
    fn default() -> Self {
        Self {
            backoff_secs: 30,
            last_attempt: None,
        }
    }
}

// Backoff doubles on every failure (30s, 60s, 120s, 240s, 480s, 960s) up to this ceiling,
// so we never wait longer than 16 minutes between retry attempts even during a long outage.
// This is more reasonable than the previous 1 hour maximum.
const MAX_BACKOFF: u64 = 960; // 16 minutes

// BUFFER-004: persistent retry state across daemon restarts.
// We save the backoff state to disk so that if the daemon restarts, it continues
// the backoff instead of immediately hammering the failing service again.
fn save_retry_state() {
    // We use a Result here but don't panic on failure, since the daemon
    // can still run without persistent state.
    let _ = (|| -> Result<()> {
        let state = RETRY_STATE
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to lock retry state"))?;
        
        let persistent = PersistentRetryState {
            backoff_secs: state.backoff_secs,
            last_attempt: state.last_attempt.map(|_| chrono::Utc::now()),
        };
        
        let path = retry_state_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(&persistent)?)?;
        Ok(())
    })();
}

fn load_retry_state() {
    let _ = (|| -> Result<()> {
        let path = retry_state_path()?;
        if !path.exists() {
            return Ok(());
        }
        
        let content = std::fs::read_to_string(path)?;
        let persistent: PersistentRetryState = serde_json::from_str(&content)?;
        
        let mut state = RETRY_STATE
            .lock()
            .map_err(|_| anyhow::anyhow!("failed to lock retry state"))?;
        
        state.backoff_secs = persistent.backoff_secs;
        state.last_attempt = persistent.last_attempt.map(|dt| {
            // Convert from UTC datetime back to Instant
            // We approximate by using the current time minus the elapsed duration
            let elapsed = chrono::Utc::now().signed_duration_since(dt);
            let elapsed_secs = elapsed.num_seconds();
            if elapsed_secs > 0 {
                Instant::now() - Duration::from_secs(elapsed_secs as u64)
            } else {
                Instant::now()
            }
        });
        
        Ok(())
    })();
}

fn retry_state_path() -> Result<PathBuf> {
    let data_dir = Config::data_dir()?;
    Ok(data_dir.join(RETRY_STATE_FILE))
}

// BUFFER-004: load persistent state when the module initializes
// Using a static initializer pattern with std::sync::Once
static INIT: std::sync::Once = std::sync::Once::new();

fn init_persistent_state() {
    INIT.call_once(|| {
        load_retry_state();
    });
}

// BUFFER-004: this backoff gate used to only wrap flush_buffered_events(). The live
// path (daemon/mod.rs's do_flush(), called every flush_timer tick) had no cooldown
// at all, so during an outage every tick re-ran its own full retry ladder from
// scratch while the buffer replay separately ran its own. Making should_retry() /
// record_success() / record_failure() pub(crate) lets daemon/mod.rs check the same
// gate before attempting a live flush, so both paths share one circuit breaker
// instead of two uncoordinated ones hammering Cognee independently.
//
// Every .lock() here recovers from a poisoned mutex rather than unwrapping straight into
// a panic (see daemon::discovery's rate limiter for the same reasoning in more detail):
// worst case a poisoned lock costs us a slightly-off backoff timer, not a crash, and a
// crash here would take down the whole daemon over what's just retry bookkeeping.
pub(crate) fn should_retry() -> bool {
    init_persistent_state();
    
    let state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match state.last_attempt {
        None => true,
        Some(t) => t.elapsed() >= Duration::from_secs(state.backoff_secs),
    }
}

pub(crate) fn record_success() {
    init_persistent_state();

    let mut state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    state.backoff_secs = 30; // Reset to minimum on success

    // This used to be `Some(Instant::now())`, which was the actual bug behind the buffer
    // never draining. should_retry() is a *shared* gate: do_flush() (the live path) and
    // flush_buffered_events() (the buffer replay) both check it, back to back, on every
    // single flush tick. Setting last_attempt to "now" on success means "we just made an
    // attempt, wait backoff_secs before the next one", which is the right idea after a
    // FAILURE, but backwards after a SUCCESS. A success means Cognee is reachable right
    // now, there's no reason to make the very next check (moments later, buffer replay's
    // turn on the same tick) wait out a fresh 30-second cooldown it didn't earn.
    //
    // Concretely: do_flush() succeeds, calls record_success(), which used to stamp
    // last_attempt = now. flush_buffered_events() runs immediately after in the same tick,
    // calls should_retry(), sees an elapsed time of a few microseconds against a 30s
    // floor, and bails. Since do_flush() succeeds on basically every tick when Cognee is
    // healthy, this reset the clock forever, and flush_buffered_events() could never
    // accumulate enough elapsed time to ever pass its own check. The buffer would fill up
    // during a real outage and then simply never drain again once things recovered, even
    // though live traffic kept flowing the whole time.
    //
    // None is the correct value here: should_retry() already treats None as "no recent
    // failure, go ahead", so a success now genuinely clears the gate for whichever path
    // (or both) checks it next, instead of quietly re-arming a cooldown nobody asked for.
    state.last_attempt = None;

    // Drop the lock before saving to disk
    drop(state);
    save_retry_state();
}

pub(crate) fn record_failure() {
    init_persistent_state();
    
    let mut state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    // Double the backoff, but cap it at MAX_BACKOFF
    state.backoff_secs = (state.backoff_secs * 2).min(MAX_BACKOFF);
    state.last_attempt = Some(Instant::now());
    // Drop the lock before saving to disk
    drop(state);
    save_retry_state();
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

/// BUFFER-004: get the current backoff seconds for health reporting
pub(crate) fn get_backoff_seconds() -> u64 {
    init_persistent_state();
    
    let state = RETRY_STATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    
    match state.last_attempt {
        None => 0,
        Some(t) => {
            let elapsed = t.elapsed().as_secs();
            if elapsed >= state.backoff_secs {
                0 // Backoff period has passed
            } else {
                state.backoff_secs - elapsed
            }
        }
    }
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
    
    #[test]
    fn test_persistent_retry_state_roundtrip() {
        let state = PersistentRetryState {
            backoff_secs: 120,
            last_attempt: Some(chrono::Utc::now()),
        };

        let serialized = serde_json::to_string(&state).unwrap();
        let deserialized: PersistentRetryState = serde_json::from_str(&serialized).unwrap();

        assert_eq!(deserialized.backoff_secs, state.backoff_secs);
        assert!(deserialized.last_attempt.is_some());
    }

    // This is the actual bug: do_flush() (the live path, in daemon/mod.rs) and
    // flush_buffered_events() (the buffer replay) share this exact gate, and on every
    // flush tick they run back to back, do_flush() first, then flush_buffered_events()
    // moments later. Before this fix, record_success() stamped last_attempt with "now",
    // so a live flush succeeding (which it does on basically every tick once Cognee's
    // healthy) would re-arm a fresh 30-second cooldown a heartbeat before the buffer
    // replay's own should_retry() check ran, and that check would always see an elapsed
    // time of basically zero and always bail. The buffer would fill up during a real
    // outage and then simply never drain again, even with live traffic flowing fine the
    // whole time. This test pins down the fix: a success has to clear the gate for
    // whoever checks next, not quietly restart it.
    //
    // RETRY_STATE is a single process-wide static, so this deliberately does the whole
    // failure-then-success sequence inside one test function rather than splitting it
    // across multiple #[test] fns, cargo test runs tests in parallel by default, and two
    // tests mutating the same global state on different threads would be a real flakiness
    // risk. Keeping it to one test keeps the whole sequence on one thread.
    #[test]
    fn test_success_clears_backoff_for_immediate_retry() {
        record_failure();
        assert!(
            !should_retry(),
            "a failure should put us in a backoff window"
        );

        record_success();
        assert!(
            should_retry(),
            "a success must clear the gate immediately, not start a fresh cooldown that \
             blocks whichever path (live flush or buffer replay) checks should_retry() next"
        );
    }
}
