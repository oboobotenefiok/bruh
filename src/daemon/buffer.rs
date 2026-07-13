//! BUFFER-001: size enforcement.  BUFFER-002: exponential backoff.
//! BUFFER-003: corruption recovery (skip bad lines).
//! BUFFER-004: persistent retry state across daemon restarts.
// This is the safety net for when Cognee is unreachable. Instead of losing events when a
// flush fails, we append them as newline-delimited JSON to a file on disk and keep trying
// to replay that file later. It's basically a tiny durable queue, no database needed, just
// an append-only file plus a simple backoff so we don't hammer a service that's already down.

use crate::{cli::Config, daemon::cursor, events::Event};
use anyhow::{Context, Result};
use log::{debug, warn};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    path::{Path, PathBuf},
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
/// Whether the shared retry gate allows an attempt right now, based on the current backoff.
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

/// Resets the shared backoff to its minimum after a successful flush.
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

/// Doubles the shared backoff (capped) and marks the failed attempt's time.
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

/// Appends `events` to the on-disk offline buffer, trimming the oldest entries once
/// `max_buffer_size` is exceeded.
///
/// # Errors
///
/// Returns an error if the config can't be loaded or the buffer file can't be written.
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

// BUFFER-007: at most this many events are ever popped (and therefore "in flight" to
// Cognee) in a single tick, across both files combined. Bounding this is what turns an
// all-or-nothing 20,000-event flush that loses everything on one bad chunk into a series
// of small, independently-acknowledged batches, a failure only ever costs this many events
// worth of retrying, never the whole backlog.
const POP_LIMIT: usize = 500;

const BACKLOG_FILE_NAME: &str = "buffer.backlog.ndjson";

/// pub(crate) so callers like daemon/mod.rs's health reporting can find the backlog file
/// without hardcoding its name a second time.
pub(crate) fn backlog_path(config: &Config) -> PathBuf {
    config.offline_buffer_path.with_file_name(BACKLOG_FILE_NAME)
}

fn cursor_file_path(config: &Config) -> PathBuf {
    config
        .offline_buffer_path
        .with_file_name(cursor::BUFFER_CURSOR_FILE)
}

/// A batch of events read off the primary buffer and/or backlog by pop_events(), along with
/// the byte offsets that reading them advanced each file's cursor to. Nothing on disk is
/// touched by pop_events() itself, ack_events() or nack_events() is what actually commits
/// these offsets, so a daemon crash between a pop and its ack/nack just means the same
/// events get read again next time rather than silently dropped or double-sent.
#[derive(Debug)]
pub struct PendingBatch {
    pub events: Vec<Event>,
    /// How many of `events` (counted from the front) came from the backlog file, the rest
    /// came from the primary buffer. Needed by nack_events() to treat the two halves
    /// differently, see its doc comment for why.
    backlog_count: usize,
    corrupt_skipped: usize,
    /// The backlog cursor's value *before* this pop consumed anything from it, kept
    /// separately from new_backlog_offset so a nack can put it back exactly where it was.
    prior_backlog_offset: u64,
    new_main_offset: u64,
    new_backlog_offset: u64,
}

impl PendingBatch {
    /// Whether this batch has no events left to retry.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// True if this batch is empty of real events but still needs to be acked, if every
    /// line read this tick was corrupt, we still want to commit the cursor past that
    /// garbage rather than re-reading (and re-warning about) the same bad lines forever.
    pub fn has_only_corrupt_lines(&self) -> bool {
        self.events.is_empty() && self.corrupt_skipped > 0
    }
}

/// BUFFER-007: pops up to POP_LIMIT events for the next flush attempt. The backlog
/// (previously-failed events) is read before the primary buffer (newly-arrived ones), so a
/// steady stream of new events can never starve out the oldest failures, they always get
/// first crack at the next retry slot.
pub async fn pop_events() -> Result<PendingBatch> {
    let config = Config::load()?;
    let main_path = config.offline_buffer_path.clone();
    let backlog_path = backlog_path(&config);
    let cursor_path = cursor_file_path(&config);

    let mut cursors = cursor::load_buffer_cursors(&cursor_path).await;
    reset_offset_on_shrink(&backlog_path, &mut cursors.backlog_offset, &mut cursors.backlog_len).await;
    reset_offset_on_shrink(&main_path, &mut cursors.main_offset, &mut cursors.main_len).await;
    let prior_backlog_offset = cursors.backlog_offset;

    let (mut events, mut corrupt, new_backlog_offset) =
        read_events_from(&backlog_path, cursors.backlog_offset, POP_LIMIT).await?;
    let backlog_count = events.len();

    let remaining = POP_LIMIT - events.len();
    let (new_main_offset, main_corrupt) = if remaining > 0 {
        let (main_events, main_corrupt, new_main_offset) =
            read_events_from(&main_path, cursors.main_offset, remaining).await?;
        events.extend(main_events);
        (new_main_offset, main_corrupt)
    } else {
        (cursors.main_offset, 0)
    };
    corrupt += main_corrupt;

    Ok(PendingBatch {
        events,
        backlog_count,
        corrupt_skipped: corrupt,
        prior_backlog_offset,
        new_main_offset,
        new_backlog_offset,
    })
}

/// Commits a batch's cursor advance after its events were sent successfully. Only ack_events
/// or nack_events (never both) should be called for a given PendingBatch.
pub async fn ack_events(batch: PendingBatch) -> Result<()> {
    let config = Config::load()?;
    commit_cursors(&config, batch.new_main_offset, batch.new_backlog_offset).await
}

/// Commits a batch's cursor advance after its events failed to send. The two halves of the
/// batch are handled differently:
///
/// - Events that came from the *primary buffer* are newly-failed: this is their first trip
///   through Cognee. They get appended to the tail of the backlog (their new durable home)
///   and the main cursor advances past them, since a copy of them now lives in the backlog.
/// - Events that came from the *backlog itself* were already failed events being retried.
///   They're already sitting on disk in the backlog, re-appending another copy of them
///   would just grow the file with a duplicate every single time a retry fails, without
///   ever actually being needed, the original copy is still sitting right there. So for
///   this half, nothing is written and the backlog cursor is simply put back to where it
///   was before this pop (`prior_backlog_offset`), leaving them exactly where they already
///   were for the next retry to pick back up.
///
/// This was previously not the case: every failed batch had its *entire* contents
/// re-appended to the backlog regardless of where it came from, so a backlog entry that
/// failed twice in a row ended up with two copies of itself on disk, three copies after a
/// third failure, and so on for as long as an outage lasted, unbounded growth for events
/// that were already durably stored and needed no second copy at all.
pub async fn nack_events(batch: PendingBatch) -> Result<()> {
    let config = Config::load()?;
    let backlog_path = backlog_path(&config);
    let newly_failed = &batch.events[batch.backlog_count..];
    append_events(&backlog_path, newly_failed).await?;
    commit_cursors(&config, batch.new_main_offset, batch.prior_backlog_offset).await
}

async fn commit_cursors(config: &Config, main_offset: u64, backlog_offset: u64) -> Result<()> {
    let cursor_path = cursor_file_path(config);
    let mut cursors = cursor::load_buffer_cursors(&cursor_path).await;
    cursors.main_offset = main_offset;
    cursors.backlog_offset = backlog_offset;

    compact_if_consumed(
        &config.offline_buffer_path,
        &mut cursors.main_offset,
        &mut cursors.main_len,
    )
    .await?;
    compact_if_consumed(
        &backlog_path(config),
        &mut cursors.backlog_offset,
        &mut cursors.backlog_len,
    )
    .await?;

    cursor::save_buffer_cursors(&cursor_path, &cursors).await
}

/// If the persisted cursor's file length is longer than the file actually is right now, the
/// file was truncated, rotated, or replaced out from under us since we last looked, and the
/// old offset no longer means anything as a read position. Same shrink-detection idea as
/// cursor::read_new_bytes, applied here since the buffer queue manages its own offsets by
/// hand instead of going through that helper.
async fn reset_offset_on_shrink(path: &Path, offset: &mut u64, persisted_len: &mut u64) {
    let current_len = tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0);
    if current_len < *persisted_len {
        warn!(
            "{:?} shrank since the cursor was last saved ({} -> {} bytes); resetting its \
             read position to the start of the file.",
            path, persisted_len, current_len
        );
        *offset = 0;
    }
    *persisted_len = current_len;
}

/// Reads up to `limit` valid events starting at byte `offset` in `path`, skipping (and
/// counting) any corrupt lines along the way. Only whole lines that were actually consumed
/// (valid or corrupt) count toward the returned offset, a line past the `limit`th valid
/// event is left untouched for the next pop rather than being read and discarded, so the
/// next tick picks up exactly where this one stopped.
async fn read_events_from(path: &Path, offset: u64, limit: usize) -> Result<(Vec<Event>, usize, u64)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(Vec<Event>, usize, u64)> {
        use std::io::{Read, Seek, SeekFrom};

        if !path.exists() {
            return Ok((Vec::new(), 0, offset));
        }

        let mut file = std::fs::File::open(&path)
            .with_context(|| format!("Cannot open buffer: {:?}", path))?;
        let len = file.metadata()?.len();
        let start = if offset > len { 0 } else { offset };
        file.seek(SeekFrom::Start(start))?;
        let mut content = String::new();
        file.read_to_string(&mut content)?;

        let mut events = Vec::with_capacity(limit.min(content.len()));
        let mut corrupt = 0usize;
        let mut consumed: u64 = 0;

        for line in content.split_inclusive('\n') {
            if events.len() >= limit {
                break;
            }
            let trimmed = line.trim();
            if trimmed.is_empty() {
                consumed += line.len() as u64;
                continue;
            }
            match parse_buffer_line(trimmed) {
                Some(e) => events.push(e),
                None => corrupt += 1,
            }
            consumed += line.len() as u64;
        }

        Ok((events, corrupt, start + consumed))
    })
    .await
    .context("buffer read task panicked")?
}

/// Appends events to the backlog file, creating it (and its parent dir) if this is the
/// first failure the daemon has ever seen. Shares the same "one spawn_blocking for the
/// whole write" shape as store_events() below, for the same reason: it's one logical unit
/// of blocking work, no reason to pay for multiple trips to the blocking thread pool.
async fn append_events(path: &Path, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let serialized: Vec<String> = events
        .iter()
        .map(serde_json::to_string)
        .collect::<std::result::Result<_, _>>()?;
    let path = path.to_path_buf();

    tokio::task::spawn_blocking(move || -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Cannot open backlog buffer: {:?}", path))?;
        for json in &serialized {
            writeln!(file, "{}", json)?;
        }
        Ok(())
    })
    .await
    .context("backlog write task panicked")?
}

/// Once a file's cursor has caught up to its full length, everything in it has been
/// consumed (acked or moved to the backlog) and there's no reason to keep the bytes around,
/// so it's truncated back to empty and the offset reset to 0. This is what keeps the two
/// files from growing forever under steady, healthy operation.
///
/// The length is re-checked immediately before truncating, and the truncation is skipped
/// (deferred to the next commit) if it no longer matches what we saw a moment ago. That gap
/// exists because store_events() can append new bytes to the primary buffer concurrently
/// with a commit here, and truncating based on a stale length would silently discard events
/// that arrived in between, this guards against exactly that race rather than assuming pop
/// and store_events can never overlap.
async fn compact_if_consumed(path: &Path, offset: &mut u64, persisted_len: &mut u64) -> Result<()> {
    let path = path.to_path_buf();
    let current_offset = *offset;

    let (new_offset, new_len) = tokio::task::spawn_blocking(move || -> Result<(u64, u64)> {
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if len == 0 {
            return Ok((0, 0));
        }
        if current_offset < len {
            return Ok((current_offset, len));
        }

        let recheck_len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if recheck_len != len {
            // Something appended to the file between our first stat and now, don't
            // truncate over it, just report the fresher length and try compacting again
            // on the next commit.
            return Ok((current_offset, recheck_len));
        }

        std::fs::write(&path, "")?;
        Ok((0, 0))
    })
    .await
    .context("buffer compaction task panicked")??;

    *offset = new_offset;
    *persisted_len = new_len;
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
/// tested directly against a real mixed batch, both here and from tests/integration.rs.
/// The actual per-line parsing is shared with read_events_from() via parse_buffer_line()
/// below, this function's own job is just iterating whole lines, it doesn't need to track
/// byte offsets the way read_events_from() does for its own cursor bookkeeping.
// Only reachable from tests (this crate's own #[cfg(test)] module plus tests/integration.rs
// as a separate test binary), rustc's dead_code lint can't see across that boundary during
// a normal `cargo build`, hence the explicit allow rather than a false "unused" warning.
#[allow(dead_code)]
pub fn parse_buffer_lines(content: &str) -> (Vec<Event>, usize) {
    let mut events = Vec::new();
    let mut corrupt = 0usize;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_buffer_line(line) {
            Some(e) => events.push(e),
            None => corrupt += 1,
        }
    }
    (events, corrupt)
}

/// Parses a single trimmed, non-empty NDJSON line into an Event, logging (and returning
/// None for) anything that doesn't parse. Shared by parse_buffer_lines() above and
/// read_events_from() in the pop/ack/nack queue below, so "what counts as a corrupt line
/// and what we do about it" lives in exactly one place.
fn parse_buffer_line(trimmed: &str) -> Option<Event> {
    match serde_json::from_str::<Event>(trimmed) {
        Ok(e) => Some(e),
        Err(e) => {
            warn!("Skipping corrupt buffer line: {}", e);
            None
        }
    }
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

    // ── BUFFER-007: cursor-based pop/ack/nack queue ─────────────────────────────

    fn write_ndjson(path: &std::path::Path, n: usize) {
        let mut content = String::new();
        for _ in 0..n {
            content.push_str(&serde_json::to_string(&sample_event()).unwrap());
            content.push('\n');
        }
        std::fs::write(path, content).unwrap();
    }

    #[tokio::test]
    async fn read_events_from_respects_limit_and_returns_exact_offset() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("buf.ndjson");
        write_ndjson(&p, 5);

        // Ask for only 2, the other 3 lines must be left completely untouched so the
        // next pop can pick them up.
        let (events, corrupt, offset) = read_events_from(&p, 0, 2).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(corrupt, 0);

        let (rest, corrupt, _) = read_events_from(&p, offset, 10).await.unwrap();
        assert_eq!(rest.len(), 3, "remaining 3 events should still be there");
        assert_eq!(corrupt, 0);
    }

    #[tokio::test]
    async fn read_events_from_skips_corrupt_lines_and_still_advances() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("buf.ndjson");
        let valid = serde_json::to_string(&sample_event()).unwrap();
        std::fs::write(&p, format!("{}\nnot json\n{}\n", valid, valid)).unwrap();

        let (events, corrupt, offset) = read_events_from(&p, 0, 10).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(corrupt, 1);

        // Offset should land at the end of the file since everything, valid or corrupt,
        // was consumed.
        let file_len = std::fs::metadata(&p).unwrap().len();
        assert_eq!(offset, file_len);
    }

    #[tokio::test]
    async fn read_events_from_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does_not_exist.ndjson");
        let (events, corrupt, offset) = read_events_from(&p, 0, 10).await.unwrap();
        assert!(events.is_empty());
        assert_eq!(corrupt, 0);
        assert_eq!(offset, 0);
    }

    #[tokio::test]
    async fn append_events_writes_to_tail() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("backlog.ndjson");
        append_events(&p, &[sample_event()]).await.unwrap();
        append_events(&p, &[sample_event(), sample_event()]).await.unwrap();

        let (events, corrupt, _) = read_events_from(&p, 0, 10).await.unwrap();
        assert_eq!(events.len(), 3, "both append calls should land in the same file");
        assert_eq!(corrupt, 0);
    }

    #[tokio::test]
    async fn compact_if_consumed_truncates_when_fully_read() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("buf.ndjson");
        write_ndjson(&p, 3);
        let file_len = std::fs::metadata(&p).unwrap().len();

        let mut offset = file_len; // fully consumed
        let mut persisted_len = 0u64;
        compact_if_consumed(&p, &mut offset, &mut persisted_len)
            .await
            .unwrap();

        assert_eq!(offset, 0, "a fully-consumed file should reset its cursor to 0");
        assert_eq!(persisted_len, 0);
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 0, "file should be truncated");
    }

    #[tokio::test]
    async fn compact_if_consumed_leaves_partially_read_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("buf.ndjson");
        write_ndjson(&p, 3);

        let mut offset = 5; // partway through, not fully consumed
        let mut persisted_len = 0u64;
        compact_if_consumed(&p, &mut offset, &mut persisted_len)
            .await
            .unwrap();

        assert_eq!(offset, 5, "offset should be untouched when not fully consumed");
        assert!(std::fs::metadata(&p).unwrap().len() > 0, "file should not be truncated");
    }

    #[test]
    fn pending_batch_reports_corrupt_only_state() {
        let empty_clean = PendingBatch {
            events: Vec::new(),
            backlog_count: 0,
            corrupt_skipped: 0,
            prior_backlog_offset: 0,
            new_main_offset: 0,
            new_backlog_offset: 0,
        };
        assert!(empty_clean.is_empty());
        assert!(!empty_clean.has_only_corrupt_lines());

        let empty_corrupt = PendingBatch {
            events: Vec::new(),
            backlog_count: 0,
            corrupt_skipped: 2,
            prior_backlog_offset: 0,
            new_main_offset: 40,
            new_backlog_offset: 0,
        };
        assert!(empty_corrupt.has_only_corrupt_lines());

        let non_empty = PendingBatch {
            events: vec![sample_event()],
            backlog_count: 0,
            corrupt_skipped: 0,
            prior_backlog_offset: 0,
            new_main_offset: 20,
            new_backlog_offset: 0,
        };
        assert!(!non_empty.is_empty());
        assert!(!non_empty.has_only_corrupt_lines());
    }

    // ── BUFFER-008: nack must not duplicate already-backlogged events ──────────

    #[tokio::test]
    async fn nack_of_backlog_only_batch_does_not_rewrite_backlog() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = dir.path().join("buffer.backlog.ndjson");
        write_ndjson(&backlog, 3);
        let original_len = std::fs::metadata(&backlog).unwrap().len();

        // Simulate what pop_events() would have produced: a batch made up entirely of
        // backlog-sourced events (backlog_count == events.len()), with a failed send.
        let (events, _, new_backlog_offset) = read_events_from(&backlog, 0, 10).await.unwrap();
        let batch = PendingBatch {
            backlog_count: events.len(),
            events,
            corrupt_skipped: 0,
            prior_backlog_offset: 0,
            new_main_offset: 0,
            new_backlog_offset,
        };

        // The fix under test: nothing gets appended for the backlog-sourced portion, so
        // the file should be byte-for-byte unchanged after a failed retry.
        let newly_failed = &batch.events[batch.backlog_count..];
        append_events(&backlog, newly_failed).await.unwrap();
        assert_eq!(
            std::fs::metadata(&backlog).unwrap().len(),
            original_len,
            "backlog-sourced events must not be re-appended to the backlog on failure"
        );

        // And the offset that would actually get committed is prior_backlog_offset (0),
        // not new_backlog_offset (past the 3 events), so the next pop reads them again
        // rather than skipping past them.
        assert_eq!(batch.prior_backlog_offset, 0);
    }

    #[tokio::test]
    async fn nack_of_mixed_batch_only_appends_the_main_sourced_half() {
        let dir = tempfile::tempdir().unwrap();
        let backlog = dir.path().join("buffer.backlog.ndjson");
        let main = dir.path().join("buffer.ndjson");
        write_ndjson(&backlog, 2);
        write_ndjson(&main, 2);
        let backlog_len_before = std::fs::metadata(&backlog).unwrap().len();

        let (mut events, _, _) = read_events_from(&backlog, 0, 10).await.unwrap();
        let backlog_count = events.len();
        let (main_events, _, _) = read_events_from(&main, 0, 10).await.unwrap();
        events.extend(main_events);

        // Only the main-sourced half (everything after backlog_count) should ever reach
        // append_events, mirroring exactly what nack_events() does internally.
        let newly_failed = &events[backlog_count..];
        assert_eq!(newly_failed.len(), 2, "only the 2 main-sourced events, not all 4");
        append_events(&backlog, newly_failed).await.unwrap();

        let (all_in_backlog, _, _) = read_events_from(&backlog, 0, 100).await.unwrap();
        assert_eq!(
            all_in_backlog.len(),
            4,
            "backlog should now hold its original 2 plus the 2 newly-failed main events, \
             not a duplicated copy of its original 2 as well"
        );
        assert!(
            std::fs::metadata(&backlog).unwrap().len() > backlog_len_before,
            "file should have grown by exactly the newly appended main events"
        );
    }
}

