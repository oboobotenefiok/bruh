//! CORE-001: session tracking.  CORE-003: health file.  CORE-004: graceful shutdown.
//! PKG 005: record_last_command called after every shell poll tick.
// This is the heart of the whole project, the background daemon that quietly watches your
// shell, your package managers, and your git commits, then batches everything up and ships
// it to Cognee every so often. Everything else (the CLI commands) is basically just a way
// to talk to the data this file collects. If this loop dies, bruh stops being useful.

pub mod buffer;
// pub(crate) since it's an internal implementation detail shared across daemon submodules
// (packages, discovery), not something outside the crate needs.
pub(crate) mod cursor;
mod discovery;
mod git;
mod packages;
// pub(crate) rather than private: cli::watch reuses exclusion_patterns()/is_excluded() from
// here so error text captured by `bruh watch` gets the exact same secret-filtering as the
// passive shell-history poller, one implementation, not two that could drift apart.
pub(crate) mod shell;
use crate::{cli::Config, cognee::remember, events::Event};
use crate::daemon::buffer::get_backoff_seconds;
use anyhow::Result;
use chrono::{DateTime, Utc};
use log::{debug, error, info, warn};
use serde_json::json;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::time::{self, Duration};

// If more than 30 minutes pass with no activity, I treat whatever comes next as a brand new
// "session." This matters because grouping events by session is what lets recall() answer
// something like "what was I doing this morning" instead of just a flat unordered timeline.
const SESSION_GAP_SECS: i64 = 30 * 60;
// If the in-memory queue somehow grows past this before the normal flush timer fires (a
// burst of activity, say), we force a flush early rather than letting memory grow unbounded.
const QUEUE_FORCE_FLUSH: usize = 500;

// COGNEE-020: how often we're willing to kick off a graph-build (improve()) pass, measured
// separately from batch_flush_interval_seconds on purpose. Flushing (ingest via /add) and
// building the graph (cognify via improve()) used to be the exact same operation, tied to
// the exact same timer, because /remember did both at once. Splitting them means we get
// to pick a slower, calmer cadence for the expensive LLM-driven part without also slowing
// down how often raw events get safely off the daemon and onto Cognee. Five minutes is a
// reasonable floor, it's long enough that a single graph-build pass has realistically
// finished before we ask for another one, but short enough that `bruh explain`/`bruh
// stats` still feel like they're looking at recent activity.
const MIN_IMPROVE_INTERVAL_SECS: u64 = 300;

// LOG-001: how often the daemon logs its "still healthy, here's what happened" summary at
// info level. Per-flush "Flushing N events" logging (every batch_flush_interval_seconds,
// so every few minutes) was demoted to debug! specifically because it added up to constant
// terminal noise for a person just leaving the daemon running in the background, most of
// those lines carry no new information tick to tick. An hourly rollup is a middle ground:
// still gives an operator watching `RUST_LOG=info` output a periodic "yes, I'm alive and
// here's what I did" signal, without scrolling the terminal every few minutes to say it.
const HOUR_SUMMARY_INTERVAL_SECS: u64 = 60 * 60;

pub async fn run() -> Result<()> {
    info!("bruh daemon starting");
    let config = Config::load()?;
    config.validate()?;

    let poll_dur = Duration::from_secs(config.poll_interval_seconds);
    let flush_dur = Duration::from_secs(config.batch_flush_interval_seconds);

    // All of this is state that lives for as long as the daemon process does. No database,
    // no external state store, just plain variables closed over by the loop below.
    let mut session_id = new_session_id();
    let mut last_event_time: Option<DateTime<Utc>> = None;
    let mut event_queue: Vec<Event> = Vec::new();
    let start = std::time::Instant::now();
    let mut last_flush_status = "none".to_string();
    let mut last_flush_time: Option<DateTime<Utc>> = None;
    // COGNEE-020: separate clock from last_flush_time, tracked in wall-clock Instant
    // rather than DateTime since we only ever compare it to "now" for a rate limit, never
    // display it anywhere. None means "never tried yet", so the very first successful
    // flush is free to trigger a graph-build immediately.
    let mut last_improve_time: Option<std::time::Instant> = None;
    // LOG-001: hourly summary state. hour_flushed_events counts everything actually sent
    // to Cognee (live flushes plus drained buffer/backlog events) since the last summary
    // line, and cognify_succeeded_this_hour is set from inside the detached improve() spawn
    // below, an Arc<AtomicBool> because that spawn runs on its own task and needs a way to
    // report back into state the main loop owns, a plain bool captured by move wouldn't be
    // visible here once the spawned task takes ownership of its own copy.
    let mut hour_start = std::time::Instant::now();
    let mut hour_flushed_events: u64 = 0;
    let cognify_succeeded_this_hour = Arc::new(AtomicBool::new(false));

    // Two independent timers on two independent cadences. Polling (checking for new shell
    // commands, package installs, etc) happens more often than flushing (actually sending a
    // batch to Cognee over the network), because polling is cheap and local while flushing
    // costs a network round trip.
    let mut poll_timer = time::interval(poll_dur);
    let mut flush_timer = time::interval(flush_dur);

    // Git watching runs as its own spawned task rather than inside the main select! loop,
    // because git commits are event-driven (they happen when they happen, not on a poll
    // cadence) so it made more sense to give it its own listener loop entirely.
    tokio::spawn(async {
        if let Err(e) = git::listen().await {
            error!("Git error: {}", e);
        }
    });

    // CORE-004: cross-platform shutdown signal
    // Unix gives us SIGTERM and SIGINT to listen for distinctly, which matters for systemd
    // and process managers that send SIGTERM on a normal stop. Everywhere else we just fall
    // back to ctrl_c(), which is the only signal tokio guarantees cross-platform support for.
    let shutdown = async {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            // signal() only fails if the OS can't set up signal handling infrastructure at
            // all, which in practice means something is deeply wrong with the environment,
            // not something a retry or a fallback could paper over. A daemon that can't
            // reliably catch SIGTERM/SIGINT has no way to ever shut down cleanly anyway, so
            // failing loudly here at startup is more honest than limping along and hoping
            // for the best.
            let mut term = signal(SignalKind::terminate()).expect("SIGTERM");
            let mut int = signal(SignalKind::interrupt()).expect("SIGINT");
            tokio::select! { _ = term.recv() => {}, _ = int.recv() => {} }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
    };
    // tokio::pin! is needed because we're going to poll this same future repeatedly inside
    // the loop's select! below, and select! requires futures it polls more than once to be
    // pinned in place rather than moved each iteration.
    tokio::pin!(shutdown);

    info!("Daemon running — session: {}", session_id);

    loop {
        tokio::select! {
            // "biased" turns off tokio's default random branch selection and instead checks
            // branches top to bottom every time multiple are ready at once. I want that here
            // specifically so shutdown always wins a race against a poll or flush tick, we'd
            // rather exit cleanly a few milliseconds early than let another poll cycle start
            // while we're trying to shut down.
            biased;

            _ = &mut shutdown => {
                info!("Shutdown — flushing {} events…", event_queue.len());
                // Best effort final flush. If the network call fails on the way out, we
                // still don't want to lose the events, so they go to the offline buffer
                // instead of just vanishing when the process exits.
                if !event_queue.is_empty() {
                    if let Err(e) = remember(event_queue.clone()).await {
                        error!("Final flush failed: {}", e);
                        let _ = buffer::store_events(&event_queue).await;
                    }
                }
                cleanup_sockets();
                info!("Daemon exited cleanly.");
                return Ok(());
            }

            _ = poll_timer.tick() => {
                // ── Shell history ──────────────────────────────────────────
                match shell::poll_shell_history(&config).await {
                    Ok(events) => {
                        // PKG-005 FIX: capture the last command BEFORE polling packages
                        // so trigger_command is populated with the shell command that
                        // preceded each package install event.
                        // Order matters a lot here. We want to know "which shell command
                        // led to this package install" (think: you ran `npm install react`
                        // and we want to link the install event back to that exact
                        // command), so we have to record the last seen shell command before
                        // we go poll package managers below, otherwise the link would be
                        // stale or missing entirely.
                        if let Some(last_cmd) = events.iter().rev().find_map(|e| {
                            if let Event::ShellCommand(sc) = e { Some(sc.command.clone()) } else { None }
                        }) {
                            packages::record_last_command(&last_cmd);
                        }

                        for mut ev in events {
                            let ts = event_ts(&ev);
                            // This is the actual session-boundary check: if the gap since
                            // the last event we saw is bigger than SESSION_GAP_SECS, we
                            // consider this the start of a new working session and mint a
                            // fresh session id.
                            if let Some(last) = last_event_time {
                                if (ts - last).num_seconds() > SESSION_GAP_SECS {
                                    session_id = new_session_id();
                                    info!("New session: {}", session_id);
                                }
                            }
                            last_event_time = Some(ts);
                            stamp_session(&mut ev, &session_id);
                            event_queue.push(ev);
                        }
                    }
                    Err(e) => error!("Shell poll: {}", e),
                }

                // ── Package managers ──
                match packages::poll_package_managers().await {
                    Ok(evs) => for mut ev in evs {
                        stamp_session(&mut ev, &session_id);
                        event_queue.push(ev);
                    },
                    Err(e) => error!("Package poll: {}", e),
                }

                // ── Unknown manager discovery ───
                // Only bother spending the search-plus-LLM cost if the person has opted in
                // via config, discovery isn't free and not everyone wants their daemon
                // reaching out to third party LLM APIs automatically.
                if config.discovery_enabled {
                    if let Err(e) = discovery::check_unknown_commands(&config).await {
                        error!("Discovery: {}", e);
                    }
                }

                // POL-006: force flush if queue is large
                if event_queue.len() >= QUEUE_FORCE_FLUSH {
                    warn!("Queue at {}. Force-flushing.", event_queue.len());
                    hour_flushed_events += do_flush(
                        &mut event_queue, &mut last_flush_status, &mut last_flush_time
                    ).await;
                }
            }

            _ = flush_timer.tick() => {
                // Check for force flush signal before flushing
                // This resets backoff and triggers a flush attempt
                if let Ok(data_dir) = crate::cli::Config::data_dir() {
                    if let Err(e) = check_force_flush_signal(&data_dir).await {
                        error!("Force flush signal check failed: {}", e);
                    }
                }
                
                if !event_queue.is_empty() {
                    hour_flushed_events += do_flush(
                        &mut event_queue, &mut last_flush_status, &mut last_flush_time
                    ).await;
                }
                // BUFFER-007: every flush tick is also a good moment to drain whatever's
                // sitting in the offline buffer/backlog from an earlier Cognee outage,
                // piggybacking this on the existing timer instead of running a third
                // separate timer for it.
                hour_flushed_events += drain_buffer().await;

                // COGNEE-020: only bother asking for a graph-build if the last flush
                // actually sent something new (status == "success"), there's no point
                // paying for a cognify pass over a dataset that hasn't changed, and only
                // if we're past our own rate limit, so we don't fire one for every flush
                // tick now that flushing is cheap again. This runs as a detached spawn,
                // deliberately not awaited, because improve(true) tells Cognee to run
                // in the background too, but the HTTP round trip to kick it off still
                // takes a moment, and that's not a moment worth blocking the poll/flush
                // loop over.
                if last_flush_status == "success" {
                    let ready = last_improve_time
                        .map(|t| t.elapsed().as_secs() >= MIN_IMPROVE_INTERVAL_SECS)
                        .unwrap_or(true);
                    if ready {
                        last_improve_time = Some(std::time::Instant::now());
                        // LOG-001: cloning the Arc (not the bool inside it) so the spawned
                        // task can report success back into state the main loop still owns
                        // after this closure moves its own copy of the handle away.
                        let cognify_flag = cognify_succeeded_this_hour.clone();
                        tokio::spawn(async move {
                            match crate::cognee::improve(true).await {
                                Ok((succeeded, _)) => {
                                    if succeeded {
                                        cognify_flag.store(true, Ordering::Relaxed);
                                    }
                                }
                                Err(e) => debug!("Background improve trigger failed: {}", e),
                            }
                        });
                    }
                }

                write_health(start.elapsed().as_secs(), event_queue.len(),
                    &last_flush_status, last_flush_time).await;

                // LOG-001: once an hour, roll up what would otherwise be scattered debug!
                // lines into one info!-level summary, so `RUST_LOG=info` (the daemon's
                // default) still gives an operator a periodic sign of life without the
                // per-flush scroll.
                if hour_start.elapsed().as_secs() >= HOUR_SUMMARY_INTERVAL_SECS {
                    let cognify_ok = cognify_succeeded_this_hour.swap(false, Ordering::Relaxed);
                    info!(
                        "Hourly summary: {} events flushed, graph enrichment {}.",
                        hour_flushed_events,
                        if cognify_ok { "succeeded at least once" } else { "did not succeed" }
                    );
                    hour_flushed_events = 0;
                    hour_start = std::time::Instant::now();
                }
            }
        }
    }
}

/// BUFFER-007: pops up to POP_LIMIT events off the offline buffer (backlog first, then the
/// primary buffer), attempts to send them, and acks or nacks the batch depending on the
/// result. Returns how many events were successfully sent, for the caller's hourly summary
/// counter. Shares buffer::should_retry()'s circuit breaker with do_flush() so a live flush
/// and a buffer drain never hammer Cognee independently during the same outage.
async fn drain_buffer() -> u64 {
    if !buffer::should_retry() {
        debug!("Cognee backoff active — skipping buffer drain this tick.");
        return 0;
    }

    let batch = match buffer::pop_events().await {
        Ok(batch) => batch,
        Err(e) => {
            error!("Buffer pop failed: {:#}", e);
            return 0;
        }
    };

    if batch.is_empty() {
        if batch.has_only_corrupt_lines() {
            // Nothing worth sending, but the cursor still needs to move past the garbage
            // lines we skipped, or we'd re-read (and re-warn about) them every single tick.
            if let Err(e) = buffer::ack_events(batch).await {
                error!("Failed to commit cursor past corrupt buffer lines: {:#}", e);
            }
        }
        return 0;
    }

    let count = batch.events.len() as u64;
    match remember(batch.events.clone()).await {
        Ok(_) => {
            debug!("Drained {} events from the offline buffer.", count);
            buffer::record_success();
            if let Err(e) = buffer::ack_events(batch).await {
                error!("Failed to commit buffer drain cursor: {:#}", e);
            }
            count
        }
        Err(e) => {
            // {:#} shows the full cause chain, see the matching comment in do_flush() for
            // why that matters here.
            error!("Buffer drain failed: {:#}. Requeueing to backlog.", e);
            buffer::record_failure();
            if let Err(e) = buffer::nack_events(batch).await {
                error!("Failed to requeue failed buffer events to backlog: {:#}", e);
            }
            0
        }
    }
}

/// Flushes the in-memory event queue to Cognee, returning how many events were
/// successfully sent (0 on backoff or failure), for the caller's hourly summary counter.
async fn do_flush(queue: &mut Vec<Event>, status: &mut String, time: &mut Option<DateTime<Utc>>) -> u64 {
    // CORE-005: do_flush() used to attempt a network call on every single tick
    // regardless of how recently Cognee had failed, while buffer.rs separately
    // tracked its own backoff for buffer replay, two uncoordinated retry loops
    // hammering Cognee independently during an outage. Now both paths check the
    // same buffer::should_retry() gate: if we're still in the backoff window, skip
    // the network attempt entirely and persist straight to the offline buffer,
    // no point re-proving Cognee is down when we already know it is, and this
    // keeps the in-memory queue from growing unbounded while we wait.
    if !buffer::should_retry() {
        debug!(
            "Cognee backoff active — buffering {} events without a network attempt.",
            queue.len()
        );
        let _ = buffer::store_events(queue).await;
        queue.clear();
        *status = "backoff".into();
        return 0;
    }

    // LOG-001: demoted from info! to debug!. This used to fire every flush tick (every
    // batch_flush_interval_seconds, a few minutes by default) regardless of whether
    // anything interesting happened, which is exactly the kind of routine, unchanging
    // line that turns a terminal into scroll noise over a long-running daemon. The hourly
    // summary logged at the end of the flush_timer arm now carries this information at
    // info! level instead, rolled up instead of repeated.
    let count = queue.len();
    debug!("Flushing {} events", count);
    match remember(queue.clone()).await {
        Ok(_) => {
            queue.clear();
            *status = "success".into();
            *time = Some(Utc::now());
            buffer::record_success();
            count as u64
        }
        Err(e) => {
            // {:#} is anyhow's alternate Display: it prints the full cause chain
            // ("top message: cause: cause: cause") on one line instead of just the
            // outermost context message. The old {} only ever showed "Network error
            // reaching Cognee at <url>", the same text for a DNS failure, a refused
            // connection, or a timeout, with the actual underlying reqwest error (the
            // part that would actually tell you which of those it was) silently
            // dropped. reqwest's own error Display text says things like "operation
            // timed out" or "dns error" or "tcp connect error", exactly the detail
            // needed to tell "the network is down" apart from "the host doesn't
            // exist" apart from "it's just slow right now".
            error!("Flush failed: {:#}. Buffering.", e);
            let _ = buffer::store_events(queue).await;
            queue.clear();
            *status = "failed".into();
            *time = Some(Utc::now());
            buffer::record_failure();
            0
        }
    }
}

// Writes a small JSON snapshot of daemon health to disk on every flush tick. This is what
// `bruh daemon --status` reads back in cli/status.rs, it's a simple file-based IPC
// mechanism rather than an actual socket or RPC call, which felt like the right amount of
// complexity for a status check nobody needs sub-second freshness on.
async fn write_health(
    uptime: u64,
    queue_len: usize,
    flush_status: &str,
    flush_time: Option<DateTime<Utc>>,
) {
    // Config::load(), load_learned_managers(), and the buffer line count below are all
    // synchronous, std::fs-backed calls, and this whole function runs once per flush tick
    // (every batch_flush_interval_seconds) inside the daemon's main async loop. Bundling
    // them into one spawn_blocking closure moves the whole sequence onto tokio's dedicated
    // blocking thread pool, so a slow read on constrained storage doesn't stall the worker
    // thread the shutdown-signal check or another poller is trying to use at the same time.
    let flush_status = flush_status.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        // BUFFER-007: buffered_events now covers both queue files, the primary buffer
        // (events not yet attempted) and the backlog (events that already failed once and
        // are waiting to be retried), so `bruh daemon --status` reports the true total
        // still sitting on disk rather than just one half of it.
        let count_lines = |path: &std::path::Path| {
            std::fs::read_to_string(path)
                .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
                .unwrap_or(0)
        };
        let buffered = Config::load()
            .ok()
            .map(|c| {
                let backlog = buffer::backlog_path(&c);
                count_lines(&c.offline_buffer_path) + count_lines(&backlog)
            })
            .unwrap_or(0);
        let learned = crate::discovery::cache::load_learned_managers()
            .map(|m| m.len())
            .unwrap_or(0);

        let health = json!({
            "status": "running",
            "uptime_seconds": uptime,
            "events_queued": queue_len,
            "last_flush_time": flush_time.map(|t| t.to_rfc3339()),
            "last_flush_status": flush_status,
            "backoff_seconds": get_backoff_seconds(),
            "buffered_events": buffered,
            // 6 is the count of package managers we know about out of the box without
            // needing discovery at all (npm, cargo, pip, etc), plus whatever's been
            // learned on top.
            "managers_known": 6 + learned,
            "managers_learned": learned,
            // Written fresh on every single call to write_health, which fires once per
            // flush tick. `bruh daemon --status` compares this against the current time
            // to tell a live daemon apart from a stale health.json left behind by a hard
            // kill (SIGKILL, an OOM kill, a crash), none of which give cleanup_sockets()
            // a chance to run and remove the file. Without this, a dead daemon's last
            // snapshot would read as "running" forever.
            "as_of": Utc::now().to_rfc3339(),
        });

        if let Ok(path) = Config::health_file_path() {
            if let Some(p) = path.parent() {
                let _ = std::fs::create_dir_all(p);
            }
            let _ = std::fs::write(&path, health.to_string());
        }
    })
    .await;
}

// Removes the health file and the git socket on a clean shutdown, so a stale file from a
// previous run doesn't confuse `bruh daemon --status` into thinking the daemon is still
// alive when it isn't.
fn cleanup_sockets() {
    if let Ok(d) = Config::data_dir() {
        let _ = std::fs::remove_file(d.join("git.sock"));
        let _ = std::fs::remove_file(d.join("health.json"));
    }
}

// Session ids are just "session_" plus a unix timestamp. Nothing clever, just needs to be
// unique enough and sortable, which a timestamp gives us for free.
fn new_session_id() -> String {
    format!("session_{}", Utc::now().timestamp())
}

// Every Event variant carries its own timestamp field but they're not accessible through a
// shared trait, so this little match just normalizes "give me the timestamp, whatever kind
// of event this is" into one place instead of repeating this match everywhere it's needed.
fn event_ts(ev: &Event) -> DateTime<Utc> {
    match ev {
        Event::ShellCommand(e) => e.timestamp,
        Event::PackageInstall(e) => e.timestamp,
        Event::GitCommit(e) => e.timestamp,
        Event::PackageManagerProfile(e) => e.discovered_at,
    }
}

// Same idea as event_ts above but for stamping the current session id onto an event before
// it goes in the queue. PackageManagerProfile deliberately does nothing here, discovered
// package manager profiles aren't tied to a particular work session, they're closer to
// standalone reference data.
fn stamp_session(ev: &mut Event, sid: &str) {
    match ev {
        Event::ShellCommand(e) => e.session_id = Some(sid.into()),
        Event::PackageInstall(e) => e.session_id = Some(sid.into()),
        Event::GitCommit(e) => e.session_id = Some(sid.into()),
        Event::PackageManagerProfile(_) => {}
    }
}

// BUFFER-004: check for a force flush signal file and reset backoff if present
pub(crate) async fn check_force_flush_signal(data_dir: &std::path::Path) -> Result<()> {
    let signal_path = data_dir.join("flush_now");
    if !signal_path.exists() {
        return Ok(());
    }

    info!("Force flush signal detected, resetting backoff state.");

    // Read the timestamp to log when the signal was sent
    if let Ok(content) = tokio::fs::read_to_string(&signal_path).await {
        info!("Force flush signal sent at: {}", content);
    }

    // Reset the backoff state
    buffer::record_success();

    // Remove the signal file
    if let Err(e) = tokio::fs::remove_file(&signal_path).await {
        warn!("Failed to remove force flush signal file: {}", e);
    }

    info!("Backoff reset successfully. Flush will be attempted.");
    Ok(())
}

