//! DISCOVERY-003: per-manager rate limiting with HashMap in daemon state.
// This is the daemon-side trigger for the discovery pipeline living in src/discovery/.
// While that module knows HOW to figure out an unknown package manager, this file decides
// WHEN to bother trying, by scanning shell history for command patterns that look like a
// package manager we don't already know about, and by rate limiting so we don't spam
// discovery attempts (and LLM calls) for the same unknown name over and over.

use crate::cli::Config;
use crate::discovery;
use anyhow::Result;
use log::{debug, info};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// These are the package managers we understand natively without needing to ask an LLM
// about them at all. OnceLock means this list gets built exactly once, lazily, the first
// time it's needed, instead of being recomputed on every call.
static BOOTSTRAPPED: OnceLock<Vec<String>> = OnceLock::new();

fn bootstrapped() -> &'static Vec<String> {
    BOOTSTRAPPED.get_or_init(|| {
        vec!["apt", "pip", "npm", "cargo", "pkg", "brew"]
            .into_iter()
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

// Lazily initializes the HashMap inside the Mutex on first use. I could have used a
// OnceLock<Mutex<HashMap<...>>> pattern instead, honestly, but this reads a little more
// plainly to me: check if it's None, and if so, set it up.
fn init_rate_limiter() {
    let mut g = RATE_LIMITER.lock().unwrap();
    if g.is_none() {
        *g = Some(HashMap::new());
    }
}

fn should_discover(name: &str, limit_secs: u64) -> bool {
    let mut g = RATE_LIMITER.lock().unwrap();
    let map = g.as_mut().unwrap();
    match map.get(name) {
        None => true,
        Some(&last) => last.elapsed() >= Duration::from_secs(limit_secs),
    }
}

fn record_attempt(name: &str) {
    let mut g = RATE_LIMITER.lock().unwrap();
    if let Some(map) = g.as_mut() {
        map.insert(name.to_string(), Instant::now());
    }
}

// Called once per poll tick from daemon/mod.rs, but only if discovery_enabled is set.
// The general shape: for each shell history file (zsh and bash), read only the NEW lines
// since the last time we looked (tracked with a cursor file so we don't rescan the whole
// history every tick), check each new line for something that looks like an install
// command, and if the program name isn't something we already know, kick off discovery for
// it in the background.
pub async fn check_unknown_commands(config: &Config) -> Result<()> {
    init_rate_limiter();

    let learned = discovery::cache::load_learned_managers().unwrap_or_default();
    let known: std::collections::HashSet<String> = bootstrapped()
        .iter()
        .chain(learned.keys())
        .cloned()
        .collect();

    // Read recent shell history to look for unknown package-manager patterns
    let data_dir = Config::data_dir()?;
    let zsh_cursor = data_dir.join(".zsh_history.cursor");
    let bash_cursor = data_dir.join(".bash_history.cursor");

    for history_path in &[
        crate::cli::home_dir().join(".zsh_history"),
        crate::cli::home_dir().join(".bash_history"),
    ] {
        if !history_path.exists() {
            continue;
        }
        // Each shell's history file gets its own cursor file tracking how many lines we've
        // already scanned for discovery purposes. This is a separate cursor from whatever
        // shell.rs uses for ingesting commands generally, discovery only cares about "have I
        // looked for unknown managers in this line before."
        let cursor_name = format!(
            "{}.discovery_cursor",
            history_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        let cursor_path = data_dir.join(cursor_name);

        let cursor = read_cursor(&cursor_path).unwrap_or(0);
        let content = std::fs::read_to_string(history_path).unwrap_or_default();
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();

        for line in &lines[cursor..] {
            if let Some(candidate) = looks_like_package_manager(line) {
                if !known.contains(&candidate) {
                    if should_discover(&candidate, config.discovery_rate_limit_seconds) {
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
        }
        let _ = write_cursor(&cursor_path, total);
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
fn read_cursor(path: &std::path::Path) -> Option<usize> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn write_cursor(path: &std::path::Path, pos: usize) -> Result<()> {
    std::fs::write(path, pos.to_string())?;
    Ok(())
}

// A thin pass-through used by the CLI's --learn flag (cli/managers.rs) to force discovery
// for a specific manager name on demand, bypassing the rate limiter and history scanning
// entirely since the user is explicitly asking for it.
pub async fn trigger_discovery(name: &str) -> Result<()> {
    discovery::discover_manager(name).await?;
    Ok(())
}
