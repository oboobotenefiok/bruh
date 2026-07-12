//! DISCOVERY-003: per-manager rate limiting with HashMap in daemon state.
// This is the daemon-side trigger for the discovery pipeline living in src/discovery/.
// While that module knows HOW to figure out an unknown package manager, this file decides
// WHEN to bother trying, by scanning shell history for command patterns that look like a
// package manager we don't already know about, and by rate limiting so we don't spam
// discovery attempts (and LLM calls) for the same unknown name over and over.

use crate::{cli::Config, daemon::cursor, discovery};
use anyhow::Result;
use log::{debug, info};
use std::{
    collections::HashMap,
    sync::OnceLock,
    time::{Duration, Instant},
};

// These are the package managers we understand natively without needing to ask an LLM
// about them at all. OnceLock means this list gets built exactly once, lazily, the first
// time it's needed, instead of being recomputed on every call.
static BOOTSTRAPPED: OnceLock<Vec<String>> = OnceLock::new();

fn bootstrapped() -> &'static Vec<String> {
    BOOTSTRAPPED.get_or_init(|| {
        discovery::BOOTSTRAPPED_MANAGERS
            .iter()
            .map(|s| s.to_string())
            .collect()
    })
}

// Per-manager last-attempt tracking (lives for the process lifetime).
// Keyed by manager name so we can rate limit each unknown manager independently rather than
// one global cooldown, if someone's terminal history has both "foo install x" and
// "bar install y" we don't want a rate limit on foo to block us from ever trying bar.
static RATE_LIMITER: std::sync::Mutex<Option<HashMap<String, Instant>>> =
    std::sync::Mutex::new(None);

// Every .lock() call in this file recovers from a poisoned mutex via
// unwrap_or_else(|poisoned| poisoned.into_inner()) rather than unwrapping it straight into
// a panic. Poisoning only means some other thread panicked while holding the lock at some
// point, not that this HashMap's data is unsafe to keep using, and the worst case of
// recovering anyway is a slightly-off rate-limit decision, not a crash. Letting a panic
// here cascade into taking down the whole daemon over what's just a rate limiter would be a
// much worse outcome than that.
//
// Both functions below self-initialize the HashMap via get_or_insert_with rather than
// requiring some separate init call to have run first, so there's no implicit "you must
// call this before that" ordering for anyone calling into this module to get right.
fn should_discover(name: &str, limit_secs: u64) -> bool {
    let mut g = RATE_LIMITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let map = g.get_or_insert_with(HashMap::new);
    match map.get(name) {
        None => true,
        Some(&last) => last.elapsed() >= Duration::from_secs(limit_secs),
    }
}

fn record_attempt(name: &str) {
    let mut g = RATE_LIMITER
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    g.get_or_insert_with(HashMap::new)
        .insert(name.to_string(), Instant::now());
}

// Called once per poll tick from daemon/mod.rs, but only if discovery_enabled is set.
// The general shape: for each shell history file (zsh and bash), read only the NEW lines
// since the last time we looked (tracked with a cursor file so we don't rescan the whole
// history every tick), check each new line for something that looks like an install
// command, and if the program name isn't something we already know, kick off discovery for
// it in the background.
pub async fn check_unknown_commands(config: &Config) -> Result<()> {
    let learned = discovery::cache::load_learned_managers().unwrap_or_default();
    let known: std::collections::HashSet<String> = bootstrapped()
        .iter()
        .chain(learned.keys())
        .cloned()
        .collect();

    // Read recent shell history to look for unknown package-manager patterns.
    // Each history file below gets its own dynamically-named cursor further down in the
    // loop (see cursor_name), so there's no need to precompute fixed paths here.
    let data_dir = Config::data_dir()?;

    for history_path in &[
        crate::cli::home_dir().join(".zsh_history"),
        crate::cli::home_dir().join(".bash_history"),
    ] {
        if !history_path.exists() {
            continue;
        }
        // Each shell's history file gets its own cursor file tracking how far we've already
        // scanned for discovery purposes. This is a separate cursor from whatever shell.rs
        // uses for ingesting commands generally, discovery only cares about "have I looked
        // for unknown managers in this part of the file before." Same shared byte-offset
        // cursor shell.rs and packages.rs use though, rather than a separate line-counting
        // scheme that had to reread the whole file from scratch on every tick just to skip
        // past lines it had already seen.
        let cursor_name = format!(
            "{}.discovery_cursor",
            history_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        let cursor_path = data_dir.join(cursor_name);

        let byte_offset = cursor::read_cursor(&cursor_path).await;
        let (content, new_offset) = cursor::read_new_bytes(history_path, byte_offset).await?;

        for line in content.lines() {
            if let Some(candidate) = looks_like_package_manager(line) {
                if !known.contains(&candidate)
                    && should_discover(&candidate, config.discovery_rate_limit_seconds)
                {
                    info!("Discovered unknown manager in history: {}", candidate);
                    record_attempt(&candidate);
                    // Discovery runs as a detached spawned task rather than being
                    // awaited inline, because it involves a web search plus an LLM
                    // call, both slow, and we don't want to block the poll loop (and
                    // therefore delay shell/package/git polling) while we wait on that.
                    tokio::spawn({
                        let name = candidate.clone();
                        async move {
                            if let Err(e) = discovery::discover_manager(&name).await {
                                debug!("Discovery failed for {}: {}", name, e);
                            }
                        }
                    });
                }
            }
        }
        cursor::write_cursor(&cursor_path, new_offset).await?;
    }

    Ok(())
}

/// Heuristic: if a line looks like `<cmd> install|add <pkg>`, return `<cmd>`.
// Deliberately simple pattern matching rather than anything fancy. We're just looking for
// "word, then an install-ish verb" and filtering out anything that looks like a path
// (contains '/') or is suspiciously long for a program name (20+ chars, almost certainly
// not a CLI tool name). False negatives are fine here, we'd rather miss a real package
// manager occasionally than trigger discovery on garbage.
fn looks_like_package_manager(line: &str) -> Option<String> {
    // Strip zsh timestamp prefix
    // zsh history lines with extended history enabled look like ": 1234567890:0;actual cmd"
    // so we chop off everything up to and including the first semicolon when we see that
    // leading ": " marker.
    let cmd = if line.starts_with(": ") {
        line.find(';').map(|i| &line[i + 1..]).unwrap_or(line)
    } else {
        line
    }
    .trim();

    let install_verbs = ["install", "add", "i", "get"];
    let mut parts = cmd.split_whitespace();
    let prog = parts.next()?;
    let verb = parts.next()?;

    if install_verbs.contains(&verb) && !prog.contains('/') && prog.len() < 20 {
        return Some(prog.to_string());
    }
    None
}

// Cursor files just hold a plain integer, "how many lines of this history file have I
// already scanned." Reading one that doesn't exist or doesn't parse just gets treated as
// "start from the beginning" via unwrap_or(0) at the call site.

