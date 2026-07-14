//! GIT-001: polling fallback.  GIT-002: branch.  GIT-003: diff_summary.
//! POLISH-004: Windows uses drop-file IPC instead of Unix socket.
// Git commits get their own listener rather than being picked up on the regular poll timer
// like shell history and package events. Reason being, commits are naturally event-driven
// (the post-commit hook fires the instant a commit happens) so real-time delivery makes
// sense here in a way it doesn't for polling a history file. That said, hooks can fail to
// fire or not be installed at all, so we layer three delivery mechanisms defensively: a
// Unix socket for instant delivery, a drop-file as a cross-platform fallback, and a raw
// `git log` poll as the ultimate safety net that works even if nothing else does.

use crate::{
    cli::Config,
    events::{Event, GitCommitEvent},
};
use anyhow::{Context, Result};
use chrono::Utc;
use log::{debug, error, info};
use serde_json::Value;

/// Entry point spawned as a Tokio task from daemon/mod.rs.
/// On Unix: Unix socket listener + drop-file poller + git-log poller.
/// On Windows: drop-file poller + git-log poller only.
pub async fn listen() -> Result<()> {
    // Always start the git log polling fallback (GIT-001)
    // This one runs regardless of platform or hook setup, worst case it catches commits
    // within 60 seconds even if every other delivery path is broken.
    tokio::spawn(poll_git_log_loop());

    // Always poll the drop-file (works as fallback on Unix too)
    tokio::spawn(poll_drop_file_loop());

    #[cfg(unix)]
    {
        // Unix socket for real-time delivery from the post-commit hook
        // This is the fast path. The post-commit hook (installed by `bruh init`, see
        // hooks/post-commit) writes a JSON payload straight into this socket the instant a
        // commit happens, so the daemon can pick it up basically immediately rather than
        // waiting on the next poll cycle.
        use tokio::net::UnixListener;

        let socket_path = Config::data_dir()?.join("git.sock");
        // A stale socket file left over from a previous (possibly crashed) daemon run will
        // make bind() fail, so we clear it out first if it exists.
        if socket_path.exists() {
            let _ = std::fs::remove_file(&socket_path);
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("Failed to bind git socket: {:?}", socket_path))?;
        info!("Git socket listening on {:?}", socket_path);

        loop {
            match listener.accept().await {
                Ok((mut stream, _)) => {
                    // Each connection gets its own spawned task so a slow or misbehaving
                    // hook invocation can't block the listener from accepting the next one.
                    tokio::spawn(async move {
                        use tokio::io::{AsyncBufReadExt, BufReader};
                        let reader = BufReader::new(&mut stream);
                        let mut lines = reader.lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            if let Ok(ev) = parse_git_payload(&line) {
                                send_event(ev).await;
                            }
                        }
                    });
                }
                Err(e) => error!("Git socket accept error: {}", e),
            }
        }
    }

    #[cfg(not(unix))]
    {
        // Windows: no Unix socket, just run forever (tasks above do the work)
        // Windows doesn't have Unix domain sockets in the same way, and I didn't want to
        // pull in named pipes just for this, so on Windows we rely entirely on the
        // drop-file and git-log fallbacks spawned above. This future just needs to never
        // resolve so the calling tokio::spawn in daemon/mod.rs stays alive.
        std::future::pending::<()>().await;
        Ok(())
    }
}

// ── Drop-file poller (cross-platform fallback) ────
// The idea here: the post-commit hook (or anything else) can append a JSON line to a known
// file instead of talking to a socket, and we just poll that file every 10 seconds looking
// for new content. Much simpler to implement portably than sockets, at the cost of a small
// delivery delay.

async fn poll_drop_file_loop() {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(10));
    loop {
        interval.tick().await;
        if let Err(e) = poll_drop_file_once().await {
            debug!("Drop-file poll: {}", e);
        }
    }
}

async fn poll_drop_file_once() -> Result<()> {
    let path = Config::git_events_path()?;

    // GIT-004: this used to be read_to_string() then write(path, "") as two separate
    // operations. If the post-commit hook's append landed in the narrow window between
    // those two calls, that commit's line would get silently wiped by the truncate, since
    // it was written after the read but destroyed before it could ever be read back. An
    // atomic rename claims the whole file in one step instead of two: whatever's at `path`
    // the instant the rename happens becomes ours to process, and the hook is free to
    // create a brand new file at the original path immediately afterward (its `mkdir -p`
    // recreates the parent, and >> just starts a fresh file if none exists), with no shared
    // window where both sides are touching the same file's content at once.
    //
    // This mostly matters for correctness on principle rather than in practice: even if
    // this race were somehow hit, the git-log poll fallback a bit further down in this file
    // is a completely independent path that would pick up the same commit within 60
    // seconds regardless, that's the whole point of having three delivery paths.
    let processing_path = path.with_extension("ndjson.processing");
    if tokio::fs::rename(&path, &processing_path).await.is_err() {
        // Nothing to claim, either the file didn't exist (nothing written since last poll)
        // or another poll tick already claimed it a moment ago. Either way, no work to do.
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&processing_path).await?;
    let _ = tokio::fs::remove_file(&processing_path).await;

    if content.trim().is_empty() {
        return Ok(());
    }

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match parse_git_payload(line) {
            Ok(ev) => send_event(ev).await,
            Err(e) => debug!("Bad git drop-file line: {}", e),
        }
    }
    Ok(())
}

// ── Git log polling fallback (GIT-001) ─────
// This is the "works no matter what" fallback. Every 60 seconds we just ask git directly
// for the last 20 commits and diff against a set of hashes we've already seen and
// processed, so it doesn't matter if the hook was never installed or the socket/drop-file
// paths both failed somehow, commits will still surface here eventually.

async fn poll_git_log_loop() {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_secs(60));
    loop {
        interval.tick().await;
        if let Err(e) = poll_git_log_once().await {
            debug!("git log poll: {}", e);
        }
    }
}

async fn poll_git_log_once() -> Result<()> {
    let data_dir = Config::data_dir()?;
    let seen_path = data_dir.join("git_seen_hashes.json");

    // We keep a persisted set of commit hashes we've already turned into events, so
    // restarting the daemon doesn't cause us to re-ingest the same commits again. Reading
    // this is synchronous std::fs work, bundled into one spawn_blocking closure alongside
    // creating the data dir, same reasoning as everywhere else in the daemon: don't block
    // an async worker thread on disk I/O when tokio's blocking pool exists for exactly this.
    let read_seen_path = seen_path.clone();
    let mut seen: std::collections::HashSet<String> =
        tokio::task::spawn_blocking(move || -> Result<std::collections::HashSet<String>> {
            std::fs::create_dir_all(&data_dir)?;
            if read_seen_path.exists() {
                Ok(
                    serde_json::from_str(&std::fs::read_to_string(&read_seen_path)?)
                        .unwrap_or_default(),
                )
            } else {
                Ok(std::collections::HashSet::new())
            }
        })
        .await
        .context("seen-hashes read task panicked")??;

    let out = tokio::process::Command::new("git")
        .args(["log", "--format=%H|%s", "-20"])
        .output()
        .await;
    let output = match out {
        Ok(o) if o.status.success() => o,
        // If we're not in a git repo, or git isn't installed, or anything else goes wrong,
        // we just quietly do nothing this tick rather than erroring the daemon out.
        _ => return Ok(()),
    };

    let branch = current_branch().await;
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut parts = line.splitn(2, '|');
        let (hash, msg) = match (parts.next(), parts.next()) {
            (Some(h), Some(m)) => (h.trim(), m.trim()),
            _ => continue,
        };
        if seen.contains(hash) {
            continue;
        }

        let event = Event::GitCommit(GitCommitEvent {
            timestamp: Utc::now(),
            hash: hash.to_string(),
            message: msg.to_string(),
            branch: branch.clone(),
            files_changed: changed_files(hash).await,
            session_id: None,
            working_directory: std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string()),
            diff_summary: diff_summary(hash).await,
        });
        send_event(event).await;
        seen.insert(hash.to_string());
    }

    let write_seen_path = seen_path.clone();
    let serialized = serde_json::to_string(&seen)?;
    tokio::task::spawn_blocking(move || std::fs::write(&write_seen_path, serialized))
        .await
        .context("seen-hashes write task panicked")??;
    Ok(())
}

// ── Shared helpers ──────────
// Both the socket path and the drop-file path end up calling this to turn a raw JSON
// payload (from the hook) into a proper GitCommitEvent. Every field pull uses a fallback
// default rather than propagating a parse error, a malformed field shouldn't nuke the
// whole event, we'd rather ingest something incomplete than nothing at all.

fn parse_git_payload(line: &str) -> Result<Event> {
    let v: Value = serde_json::from_str(line).context("Bad JSON in git payload")?;
    Ok(Event::GitCommit(GitCommitEvent {
        timestamp: Utc::now(),
        hash: v["hash"].as_str().unwrap_or("").to_string(),
        message: v["message"].as_str().unwrap_or("").to_string(),
        branch: v["branch"].as_str().unwrap_or("").to_string(),
        files_changed: v["files_changed"]
            .as_array()
            .unwrap_or(&vec![])
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.to_string()))
            .collect(),
        session_id: None,
        working_directory: v["working_directory"].as_str().map(|s| s.to_string()),
        diff_summary: v["diff_summary"].as_str().map(|s| s.to_string()),
    }))
}

// Shared by all three delivery paths (socket, drop-file, git-log poll): try to send the
// event straight to Cognee, and if that fails for any reason, fall back to the same offline
// buffer everything else uses so we don't lose the commit event.
async fn send_event(event: Event) {
    if let Err(e) = crate::cognee::remember_single(event.clone()).await {
        error!("Git event ingest failed: {}", e);
        let _ = crate::daemon::buffer::store_events(&[event]).await;
    } else {
        debug!("Git commit ingested");
    }
}

// Shells out to git rather than parsing .git/HEAD ourselves, less code and it handles
// detached HEAD and other edge cases correctly for free.
//
// tokio::process::Command rather than std::process::Command: this runs inside the daemon's
// async event loop, and spawning a child process plus waiting for it to exit is exactly the
// kind of thing that can take a noticeable moment, especially on the slower storage some of
// this project's target devices have. Using tokio's own async-native process API means that
// wait happens without parking one of the runtime's limited worker threads for the duration,
// so the shutdown-signal check and other pollers stay responsive while git runs.
async fn current_branch() -> String {
    tokio::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".into())
}

// GIT-003: grabs just the last line of `git show --stat`, which is the "N files changed,
// M insertions(+), K deletions(-)" summary line git prints. That one line is plenty for
// recall() to answer "what did that commit touch" without us needing the full diff.
async fn diff_summary(hash: &str) -> Option<String> {
    let out = tokio::process::Command::new("git")
        .args(["show", "--stat", "--format=", hash])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .last()
        .map(|l| l.trim().to_string())
}

// Full list of files touched by a commit, used alongside diff_summary so recall() can
// answer more specific questions like "did I touch main.rs in that commit."
async fn changed_files(hash: &str) -> Vec<String> {
    tokio::process::Command::new("git")
        .args(["diff-tree", "--no-commit-id", "-r", "--name-only", hash])
        .output()
        .await
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}
