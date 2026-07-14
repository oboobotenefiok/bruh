//! Shared byte-offset cursor persistence, used anywhere the daemon needs to remember "how
//! far into this file did we already read" between poll ticks or daemon restarts.
//!
//! This used to be two different strategies living side by side in the daemon: shell.rs
//! tracked a byte offset and seeked straight to it, while packages.rs's dpkg log tailing
//! and daemon/discovery.rs's unknown-command scanning each tracked a plain line count and
//! re-read the WHOLE file from the start on every single tick just to skip past the lines
//! they'd already seen. Byte-offset seeking is strictly better for this: it only reads the
//! bytes that are actually new since last time, so a log file that's grown to megabytes
//! over weeks doesn't cost more to poll than one that was created five minutes ago. This
//! module is the one place that strategy lives now, so anything that needs "read only what's
//! new" reads from here instead of reinventing it with a different (worse) approach.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Reads the byte offset saved at `cursor_path`, or 0 if there isn't one yet, or its
/// contents don't parse as a number. A missing or corrupt cursor just means "start reading
/// from the beginning of the file," not a hard failure, consistent with how the rest of the
/// daemon treats corrupt local state everywhere else.
pub async fn read_cursor(cursor_path: &Path) -> u64 {
    tokio::fs::read_to_string(cursor_path)
        .await
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Persists the byte offset so the next poll tick (or the daemon after a restart) picks up
/// exactly where this one left off.
pub async fn write_cursor(cursor_path: &Path, offset: u64) -> Result<()> {
    tokio::fs::write(cursor_path, offset.to_string()).await?;
    Ok(())
}

/// Reads only the bytes of `path` from `cursor` onward, seeking straight there instead of
/// reading the whole file and throwing away everything before the cursor. If the file has
/// shrunk since we last looked (truncated, rotated out from under us, or replaced with a
/// fresh one) the old cursor no longer makes sense as a seek position, so this resets to the
/// start rather than seeking past the end of a now-smaller file. Returns the new content
/// plus the file's current total length, which is exactly what the caller should persist as
/// its next cursor.
///
/// The actual seek-and-read happens inside spawn_blocking. File I/O like this is
/// synchronous at the OS level no matter what, doing it directly on an async worker thread
/// would block that thread (and everything else scheduled on it) for however long the read
/// takes, spawn_blocking moves the work to tokio's dedicated blocking thread pool instead.
pub async fn read_new_bytes(path: &Path, cursor: u64) -> Result<(String, u64)> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<(String, u64)> {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&path)?;
        let len = file.metadata()?.len();
        let start = if cursor > len { 0 } else { cursor };
        file.seek(SeekFrom::Start(start))?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        Ok((buf, len))
    })
    .await?
}

/// BUFFER-007: cursor state for the two-file (primary + backlog) buffer queue in
/// daemon/buffer.rs. This is a different shape from the plain byte-offset files above
/// (shell.rs's `.cursor` files, packages.rs's dpkg log cursor), those only ever need to
/// track one offset into one file, so a bare number on disk is enough. The buffer queue
/// needs to track two offsets (one per file) together, plus the file length each was saved
/// at, so a restart can tell "the file is shorter than last time, someone truncated or
/// rotated it out from under us" apart from "everything's fine, just hasn't grown since our
/// last read", the same distinction read_new_bytes() above makes for a single file. Bundling
/// all four fields into one JSON file (rather than four separate small files) also means one
/// read and one write per pop/ack/nack instead of four, and no risk of the two offsets ever
/// getting persisted out of sync with each other if a write is interrupted partway through.
pub const BUFFER_CURSOR_FILE: &str = "cursor.json";

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct BufferCursors {
    /// Byte offset already consumed from the primary buffer (buffer.ndjson).
    pub main_offset: u64,
    /// Byte offset already consumed from the backlog buffer (buffer.backlog.ndjson).
    pub backlog_offset: u64,
    /// Primary buffer's length as of the last time this cursor was saved, used to detect
    /// the file having shrunk out from under us since then.
    pub main_len: u64,
    /// Backlog buffer's length as of the last time this cursor was saved, same purpose.
    pub backlog_len: u64,
}

/// Loads the persisted buffer cursors, or a fresh all-zero BufferCursors if the file is
/// missing or doesn't parse. Same "corrupt or missing local state just means start over"
/// philosophy as read_cursor() above, a lost cursor here means re-reading buffered events
/// that may have already been sent, not losing any, so it's the safe direction to fail in.
pub async fn load_buffer_cursors(path: &Path) -> BufferCursors {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => BufferCursors::default(),
    }
}

/// Persists the buffer cursors so the next flush tick (or the daemon after a restart)
/// resumes reading exactly where it left off in both files.
pub async fn save_buffer_cursors(path: &Path, cursors: &BufferCursors) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(path, serde_json::to_string_pretty(cursors)?).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cursor_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.cursor");
        write_cursor(&p, 4242).await.unwrap();
        assert_eq!(read_cursor(&p).await, 4242);
    }

    #[tokio::test]
    async fn missing_cursor_reads_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does_not_exist.cursor");
        assert_eq!(read_cursor(&p).await, 0);
    }

    #[tokio::test]
    async fn corrupt_cursor_reads_as_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("garbage.cursor");
        tokio::fs::write(&p, "not a number").await.unwrap();
        assert_eq!(read_cursor(&p).await, 0);
    }

    #[tokio::test]
    async fn read_new_bytes_only_returns_content_past_the_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log.txt");
        tokio::fs::write(&p, "line one\nline two\n").await.unwrap();

        let (first_pass, cursor_after_first) = read_new_bytes(&p, 0).await.unwrap();
        assert_eq!(first_pass, "line one\nline two\n");

        tokio::fs::write(&p, "line one\nline two\nline three\n")
            .await
            .unwrap();
        let (second_pass, _) = read_new_bytes(&p, cursor_after_first).await.unwrap();
        assert_eq!(second_pass, "line three\n");
    }

    #[tokio::test]
    async fn buffer_cursors_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(BUFFER_CURSOR_FILE);
        let cursors = BufferCursors {
            main_offset: 100,
            backlog_offset: 50,
            main_len: 200,
            backlog_len: 75,
        };
        save_buffer_cursors(&p, &cursors).await.unwrap();
        let loaded = load_buffer_cursors(&p).await;
        assert_eq!(loaded.main_offset, 100);
        assert_eq!(loaded.backlog_offset, 50);
        assert_eq!(loaded.main_len, 200);
        assert_eq!(loaded.backlog_len, 75);
    }

    #[tokio::test]
    async fn missing_buffer_cursors_default_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does_not_exist.json");
        let loaded = load_buffer_cursors(&p).await;
        assert_eq!(loaded.main_offset, 0);
        assert_eq!(loaded.backlog_offset, 0);
    }

    #[tokio::test]
    async fn corrupt_buffer_cursors_default_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("garbage.json");
        tokio::fs::write(&p, "not json").await.unwrap();
        let loaded = load_buffer_cursors(&p).await;
        assert_eq!(loaded.main_offset, 0);
    }

    #[tokio::test]
    async fn read_new_bytes_resets_when_file_shrinks() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log.txt");
        tokio::fs::write(&p, "a fairly long line of previous content\n")
            .await
            .unwrap();
        let stale_cursor = 1_000_000u64; // pretend we'd read way more than exists now

        tokio::fs::write(&p, "short\n").await.unwrap();
        let (content, _) = read_new_bytes(&p, stale_cursor).await.unwrap();
        assert_eq!(content, "short\n");
    }
}
