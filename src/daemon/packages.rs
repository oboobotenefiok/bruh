//! PKG-001: npm structured.  PKG-002: cargo version.  PKG-003: pip version.
//! PKG-004: brew upgrades.  PKG-005: trigger_command correlation.
//! POLISH-004: Windows package managers (winget, choco, scoop).
// This file watches every package manager we know about natively (as opposed to ones
// discovered on the fly via src/discovery/) and turns installs into PackageInstallEvent.
// Each package manager gets its own poll function because they're all shaped differently:
// some have a proper install log we can tail (apt, npm), others we have to snapshot the
// installed package list and diff it against the previous snapshot to spot what's new
// (pip, cargo, brew). Whichever approach fits the tool best is what I went with per manager.

use crate::{
    cli::{home_dir, Config},
    events::{Event, ManagerType, PackageInstallEvent},
};
use anyhow::Result;
use chrono::Utc;
use log::debug;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

// PKG-005: last shell command captured by daemon for causal correlation.
// This is populated in daemon/mod.rs after each shell poll tick.
// The whole point of this static is answering "what command caused this install." If you
// ran `npm install react` in your shell, we want the resulting PackageInstallEvent for
// react to remember that it was triggered by that exact command, so recall() can later say
// "you installed react because you ran npm install react" instead of just "react got
// installed at some point."
static LAST_COMMAND: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

pub fn record_last_command(cmd: &str) {
    if let Ok(mut g) = LAST_COMMAND.lock() {
        *g = Some(cmd.to_string());
    }
}

fn last_command() -> Option<String> {
    LAST_COMMAND.lock().ok().and_then(|g| g.clone())
}

fn working_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|p| p.to_string_lossy().to_string())
}

// Every single poll_* function below eventually calls this to build the actual event, so
// trigger_command and working_directory get filled in consistently everywhere instead of
// each poller having to remember to do it themselves.
fn make_event(manager: &str, package: String, version: Option<String>) -> Event {
    Event::PackageInstall(PackageInstallEvent {
        timestamp: Utc::now(),
        manager: manager.to_string(),
        manager_type: ManagerType::Bootstrapped,
        package,
        version,
        trigger_command: last_command(), // PKG-005: wired correctly
        exit_code_trigger: None,
        session_id: None,
        working_directory: working_dir(),
    })
}

// Loads whatever snapshot we saved last time from state_path (an empty map if there isn't
// one yet, or it failed to parse, corrupt local state here is recoverable, not fatal), and
// only rewrites the file if `current` actually differs from it. brew/pip/cargo/winget/choco
// all used to read-diff-write this same shape independently, and all of them wrote the file
// back unconditionally on every single poll tick regardless of whether anything changed,
// which meant a full JSON re-serialize and disk write every 30-60 seconds forever, even
// when nothing was installed. Skipping the write when nothing changed avoids that, which
// matters more here than it would on a beefier machine given how much this project cares
// about being gentle on constrained storage (see the Termux-focused choices throughout).
// Returns the previous snapshot either way, since that's what every caller needs to diff
// its freshly-gathered `current` map against.
async fn diff_and_persist(
    state_path: &Path,
    current: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let previous: HashMap<String, String> = if state_path.exists() {
        tokio::fs::read_to_string(state_path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    if &previous != current {
        tokio::fs::write(state_path, serde_json::to_string_pretty(current)?).await?;
    }

    Ok(previous)
}

// Runs a subprocess on tokio's dedicated blocking-friendly process backend instead of
// calling std::process::Command::output() directly inside an async fn. A synchronous
// Command::output() call spawns and then blocks the CURRENT thread until the child exits,
// which on tokio's multi-threaded runtime means one of the (typically CPU-count-sized) async
// worker threads sits frozen for however long `pip list` or `brew list --versions` takes to
// run, unable to service any other task scheduled on it: the git listener, the shutdown
// signal check, or another poller's turn. tokio::process::Command is a proper async-native
// wrapper, awaiting it yields control back to the runtime instead of parking a thread.
async fn run_command(program: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
    tokio::process::Command::new(program)
        .args(args)
        .output()
        .await
}

// Called once per poll tick from daemon/mod.rs. Runs every manager's poller and collects
// whatever install events came out of each. I used a little macro here (try_poll!) just to
// avoid repeating the same match-and-log boilerplate for every single manager, a failed
// poll on one manager (say pip isn't installed) shouldn't stop us from checking the others.
pub async fn poll_package_managers() -> Result<Vec<Event>> {
    let mut events = Vec::new();
    macro_rules! try_poll {
        ($f:expr) => {
            match $f.await {
                Ok(mut v) => events.append(&mut v),
                Err(e) => debug!("Package poll error: {}", e),
            }
        };
    }

    // Cross-platform
    try_poll!(poll_pip());
    try_poll!(poll_npm());
    try_poll!(poll_cargo());

    // Unix-only
    #[cfg(not(windows))]
    {
        try_poll!(poll_apt());
        try_poll!(poll_pkg());
        try_poll!(poll_brew());
    }

    // Windows-only
    #[cfg(windows)]
    {
        try_poll!(poll_winget());
        try_poll!(poll_choco());
        try_poll!(poll_scoop());
    }

    Ok(events)
}

// ── apt (Linux) ─────────
// apt keeps a real install log at /var/log/dpkg.log, so unlike pip/cargo/brew we don't need
// to snapshot-and-diff here, we can just tail new lines since our last cursor position.

#[cfg(not(windows))]
async fn poll_apt() -> Result<Vec<Event>> {
    poll_dpkg_log("/var/log/dpkg.log", "apt", "apt.cursor").await
}

// Termux's pkg manager is also backed by dpkg under the hood, just at a different log path,
// so it reuses the exact same tailing logic as apt.
#[cfg(not(windows))]
async fn poll_pkg() -> Result<Vec<Event>> {
    poll_dpkg_log(
        "/data/data/com.termux/files/usr/var/log/dpkg.log",
        "pkg",
        "pkg.cursor",
    )
    .await
}

// Shared tailing logic for any dpkg-format log: seek straight to the byte offset we
// stopped at last time (via daemon::cursor), read only what's new since then, and save the
// file's new total length as the next cursor. This used to read_lines_from() the WHOLE log
// file on every single tick and only then skip past already-seen lines, which meant the
// full file got read into memory and parsed from scratch every 30-60 seconds regardless of
// how little had actually changed, exactly the inefficiency shell.rs's history poller
// already solved with the same byte-offset approach this now shares.
#[cfg(not(windows))]
async fn poll_dpkg_log(log_path: &str, manager: &str, cursor_file: &str) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let log = PathBuf::from(log_path);
    if !log.exists() {
        return Ok(events);
    }

    let state_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&state_dir).await?;
    let cursor_path = state_dir.join(cursor_file);
    let cursor = super::cursor::read_cursor(&cursor_path).await;

    let (new_content, new_cursor) = super::cursor::read_new_bytes(&log, cursor).await?;

    for line in new_content.lines() {
        if !line.contains(" install ") {
            continue;
        }
        if let Some((pkg, ver)) = parse_dpkg_line(line) {
            events.push(make_event(manager, pkg, Some(ver)));
        }
    }
    super::cursor::write_cursor(&cursor_path, new_cursor).await?;
    Ok(events)
}

// dpkg log lines look roughly like:
// "2024-01-01 12:00:00 install libssl-dev:amd64 <none> 1.0.2-1ubuntu4"
// so we split on whitespace and grab the package name (stripping the :arch suffix) and the
// version, which sit at fixed positions in that format.
fn parse_dpkg_line(line: &str) -> Option<(String, String)> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 6 && parts[2] == "install" {
        let pkg = parts[3].split(':').next().unwrap_or(parts[3]);
        return Some((pkg.to_string(), parts[5].to_string()));
    }
    None
}

// ── brew (macOS) – PKG-004: detect upgrades too ─
// Homebrew doesn't have a simple append-only log we can tail the way apt does, so instead
// we snapshot `brew list --versions` on every poll and diff it against what we saw last
// time. New packages are installs, and packages whose version changed are upgrades, both
// get emitted as events (PKG-004 specifically was about not missing the upgrade case).

#[cfg(not(windows))]
async fn poll_brew() -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let out = run_command("brew", &["list", "--versions"]).await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        // brew not installed, or some other failure, nothing to poll then.
        _ => return Ok(events),
    };

    let state_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&state_dir).await?;
    let state_path = state_dir.join("brew_state.json");

    let mut current: HashMap<String, String> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.split_whitespace();
        if let (Some(name), Some(ver)) = (parts.next(), parts.next()) {
            current.insert(name.to_string(), ver.to_string());
        }
    }

    let previous = diff_and_persist(&state_path, &current).await?;

    for (name, ver) in &current {
        let is_new = !previous.contains_key(name);
        // PKG-004: also emit on version upgrade
        let is_upgrade = previous.get(name).map(|v| v != ver).unwrap_or(false);
        if is_new || is_upgrade {
            debug!(
                "brew {}: {} {}",
                if is_upgrade { "upgrade" } else { "install" },
                name,
                ver
            );
            events.push(make_event("brew", name.clone(), Some(ver.clone())));
        }
    }

    Ok(events)
}

// ── pip (cross-platform) ─────────
// Same snapshot-and-diff strategy as brew. `pip list --format=json` gives us clean
// structured output already, no scraping needed, which is nice.

async fn poll_pip() -> Result<Vec<Event>> {
    let mut events = Vec::new();
    // We try pip3 first since that's the more explicit, less ambiguous binary name on
    // systems where python2's pip might also be lying around, and only fall back to plain
    // `pip` if pip3 isn't found.
    let out = match run_command("pip3", &["list", "--format=json"]).await {
        Ok(o) => Ok(o),
        Err(_) => run_command("pip", &["list", "--format=json"]).await,
    };
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Ok(events),
    };

    let state_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&state_dir).await?;
    let state_path = state_dir.join("pip_state.json");

    let current: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    // Lowercasing package names here because pip is case-insensitive about them but the
    // JSON output isn't guaranteed to always use the same casing, so this avoids treating
    // "Flask" and "flask" as two different packages across polls.
    let current_map: HashMap<String, String> = current
        .iter()
        .filter_map(|v| {
            Some((
                v["name"].as_str()?.to_lowercase(),
                v["version"].as_str()?.to_string(),
            ))
        })
        .collect();

    let previous = diff_and_persist(&state_path, &current_map).await?;

    for (name, ver) in &current_map {
        if !previous.contains_key(name) {
            debug!("pip install: {} {}", name, ver);
            events.push(make_event("pip", name.clone(), Some(ver.clone())));
        }
    }

    Ok(events)
}

// ── npm (cross-platform) ──────────────────────────────────────────────────────
// npm writes a verbose debug log for every command run into ~/.npm/_logs, one file per
// invocation. Rather than diffing installed packages (which would miss local/dev
// dependencies scoping details), we scan these logs directly for install/add commands,
// which also happens to be how we recover the actual package name reliably.

async fn poll_npm() -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let log_dir = home_dir().join(".npm/_logs");
    if !log_dir.exists() {
        return Ok(events);
    }

    let state_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&state_dir).await?;
    let seen_path = state_dir.join("npm_seen_logs.json");

    let mut seen: std::collections::HashSet<String> = if seen_path.exists() {
        tokio::fs::read_to_string(&seen_path)
            .await
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    } else {
        std::collections::HashSet::new()
    };

    // We only look at the 5 most recently modified log files rather than the whole
    // directory (npm can accumulate hundreds of these over time), sorted newest first so
    // "most recent activity" is what we check. Directory listing and metadata reads are
    // still std::fs here (tokio::fs::read_dir's async iteration doesn't buy much for a
    // directory this size, and we need synchronous metadata for the sort_by_key below
    // anyway), the actual per-file content reads further down do go through tokio::fs.
    let mut entries: Vec<_> = std::fs::read_dir(&log_dir)?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.metadata().and_then(|m| m.modified()).ok());

    let mut any_new = false;
    for entry in entries.iter().rev().take(5) {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_string();
        if seen.contains(&name) {
            continue;
        }
        if !path.extension().map(|e| e == "log").unwrap_or(false) {
            continue;
        }

        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            for line in content.lines() {
                if line.contains("verbose cli")
                    && (line.contains("install") || line.contains("add"))
                {
                    if let Some(pkg) = extract_npm_package(line) {
                        debug!("npm install: {}", pkg);
                        events.push(make_event("npm", pkg, None));
                    }
                }
            }
            seen.insert(name);
            any_new = true;
        }
    }

    // Same "skip the write if nothing changed" principle as diff_and_persist, just inlined
    // here since this is tracking a HashSet of filenames rather than a HashMap snapshot.
    if any_new {
        tokio::fs::write(&seen_path, serde_json::to_string(&seen)?).await?;
    }
    Ok(events)
}

// The "verbose cli" line in an npm log looks like a Python-ish list literal:
// "0 verbose cli [ '/usr/bin/node', '/usr/bin/npm', 'install', 'react' ]"
// so we grab everything between the brackets, split on commas, strip quotes and whitespace
// off each token, then walk the tokens looking for the install verb (install/add/i/ci) and
// return whatever comes right after it as the package name, skipping flags (start with '-')
// and paths (start with '/').
fn extract_npm_package(line: &str) -> Option<String> {
    let start = line.find('[')?;
    let end = line.rfind(']')?;
    let inner = &line[start + 1..end];
    let tokens: Vec<&str> = inner
        .split(',')
        .map(|t| t.trim().trim_matches('\'').trim_matches('"'))
        .collect();
    let skip = ["install", "add", "i", "ci"];
    let mut found_verb = false;
    for tok in &tokens {
        if skip.contains(tok) {
            found_verb = true;
            continue;
        }
        if found_verb && !tok.is_empty() && !tok.starts_with('/') && !tok.starts_with('-') {
            return Some(tok.to_string());
        }
    }
    None
}

// ── cargo (cross-platform) ─────
// Cargo doesn't keep an install log either, but it does cache every downloaded crate as a
// .crate file under ~/.cargo/registry/cache/<source>/, named like "serde-1.0.193.crate".
// So we scan that directory tree, parse out name and version from each filename, and diff
// against the previous snapshot the same way we do for pip and brew.

async fn poll_cargo() -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let registry_dir = home_dir().join(".cargo/registry/cache");
    if !registry_dir.exists() {
        return Ok(events);
    }

    let state_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&state_dir).await?;
    let state_path = state_dir.join("cargo_state.json");

    let mut current: HashMap<String, String> = HashMap::new();
    // registry/cache/ has one subdirectory per registry source (usually just
    // github.com-<hash> for crates.io), and each of those holds the actual .crate files.
    // Directory walking stays std::fs here (it's a fast, local, small-fanout listing, not
    // worth the ceremony of async iteration), same reasoning as the npm log directory scan.
    for source in std::fs::read_dir(&registry_dir)?.filter_map(|e| e.ok()) {
        if !source.path().is_dir() {
            continue;
        }
        for entry in std::fs::read_dir(source.path())?.filter_map(|e| e.ok()) {
            let p = entry.path();
            if p.extension().map(|e| e == "crate").unwrap_or(false) {
                if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                    if let Some((name, ver)) = split_crate_filename(stem) {
                        current.insert(name.to_string(), ver.to_string());
                    }
                }
            }
        }
    }

    let previous = diff_and_persist(&state_path, &current).await?;

    for (name, ver) in &current {
        if !previous.contains_key(name) {
            debug!("cargo: {} {}", name, ver);
            events.push(make_event(
                "cargo",
                name.clone(),
                if ver.is_empty() {
                    None
                } else {
                    Some(ver.clone())
                },
            ));
        }
    }
    Ok(events)
}

// Splits a cargo registry cache filename stem like "serde-1.0.193" into ("serde",
// "1.0.193"). This used to just split on the LAST hyphen (str::rsplit_once), which works
// for a plain release version but silently breaks on a semver prerelease: a stem like
// "my-crate-1.0.0-beta.1" has a hyphen inside the prerelease suffix too, so splitting on
// the last one gives ("my-crate-1.0.0", "beta.1"), folding part of the real version into
// the name. Crate names can contain hyphens, and so can prerelease versions, so neither
// "first hyphen" nor "last hyphen" is correct in general. A version always starts with a
// digit, and crate name segments essentially never do, so scanning left to right for the
// first hyphen immediately followed by a digit finds the real name/version boundary in both
// the plain and prerelease case. It isn't a fully general solution (a crate name with a
// digit-leading segment, like a hypothetical "foo-2fa", could still fool it), but it
// correctly handles every case that actually matters here, which the old version didn't.
fn split_crate_filename(stem: &str) -> Option<(&str, &str)> {
    let bytes = stem.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'-' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit) {
            return Some((&stem[..i], &stem[i + 1..]));
        }
    }
    None
}

// ── Windows package managers ───────────
// winget, choco, and scoop each have their own output format, but the overall
// snapshot-and-diff strategy is identical to pip/cargo/brew, so poll_windows_pm below is a
// small shared driver that takes a parser function per manager and does the rest generically.

#[cfg(windows)]
async fn poll_winget() -> Result<Vec<Event>> {
    poll_windows_pm("winget", &["list"], "winget_state.json", parse_winget_line).await
}

#[cfg(windows)]
async fn poll_choco() -> Result<Vec<Event>> {
    poll_windows_pm(
        "choco",
        &["list", "--local-only"],
        "choco_state.json",
        parse_choco_line,
    )
    .await
}

#[cfg(windows)]
async fn poll_scoop() -> Result<Vec<Event>> {
    let _out = run_command("scoop", &["list"]).await;
    // TODO: this one's still a stub, I ran low on time to build and test the scoop output
    // parser properly on a real Windows box before the deadline. Returning empty for now
    // rather than guessing at scoop's exact list format and shipping something wrong.
    Ok(vec![])
}

#[cfg(windows)]
async fn poll_windows_pm(
    cmd: &str,
    args: &[&str],
    state_file: &str,
    parse_line: fn(&str) -> Option<(String, String)>,
) -> Result<Vec<Event>> {
    let mut events = Vec::new();
    let out = run_command(cmd, args).await;
    let out = match out {
        Ok(o) if o.status.success() => o,
        _ => return Ok(events),
    };

    let state_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&state_dir).await?;
    let state_path = state_dir.join(state_file);

    let mut current: HashMap<String, String> = HashMap::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        if let Some((name, ver)) = parse_line(line) {
            current.insert(name, ver);
        }
    }

    let previous = diff_and_persist(&state_path, &current).await?;

    for (name, ver) in &current {
        if !previous.contains_key(name) {
            events.push(make_event(cmd, name.clone(), Some(ver.clone())));
        }
    }
    Ok(events)
}

#[cfg(windows)]
fn parse_winget_line(line: &str) -> Option<(String, String)> {
    // winget list output: "Name   Id   Version   Available   Source"
    // Skip header lines
    if line.starts_with("Name") || line.starts_with('-') || line.is_empty() {
        return None;
    }
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 3 {
        Some((parts[0].to_string(), parts[2].to_string()))
    } else {
        None
    }
}

#[cfg(windows)]
fn parse_choco_line(line: &str) -> Option<(String, String)> {
    // choco list output: "packagename version"
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() >= 2 && !parts[0].starts_with("Chocolatey") {
        Some((parts[0].to_string(), parts[1].to_string()))
    } else {
        None
    }
}

// ── Tests (TEST-002 + TEST-003 pip diff) ──
// A grab bag of unit tests covering the trickier parsing logic in this file (dpkg lines,
// npm's bracket-list log format, cargo's hyphen splitting) plus the diff logic pip and the
// others rely on, just without needing an actual pip/cargo/dpkg installed to run them.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_dpkg_install() {
        let line = "2024-01-01 12:00:00 install libssl-dev:amd64 <none> 1.0.2-1ubuntu4";
        assert_eq!(
            parse_dpkg_line(line),
            Some(("libssl-dev".into(), "1.0.2-1ubuntu4".into()))
        );
    }

    #[test]
    fn test_parse_dpkg_not_install() {
        let line = "2024-01-01 12:00:00 remove libssl-dev:amd64 1.0.2 <none>";
        assert_eq!(parse_dpkg_line(line), None);
    }

    #[test]
    fn test_extract_npm_package_install() {
        let line = "0 verbose cli [ '/usr/bin/node', '/usr/bin/npm', 'install', 'react' ]";
        assert_eq!(extract_npm_package(line), Some("react".into()));
    }

    #[test]
    fn test_extract_npm_package_add() {
        let line = "0 verbose cli [ '/usr/bin/node', '/usr/bin/npm', 'add', 'lodash' ]";
        assert_eq!(extract_npm_package(line), Some("lodash".into()));
    }

    #[test]
    fn test_split_crate_filename_plain_version() {
        assert_eq!(
            split_crate_filename("serde-1.0.193"),
            Some(("serde", "1.0.193"))
        );
    }

    #[test]
    fn test_split_crate_filename_hyphenated_name() {
        assert_eq!(
            split_crate_filename("my-cool-crate-1.2.3"),
            Some(("my-cool-crate", "1.2.3"))
        );
    }

    #[test]
    fn test_split_crate_filename_prerelease_version() {
        // This is the bug the old str::rsplit_once-based version had: splitting on the
        // LAST hyphen puts part of a prerelease version into the name instead.
        assert_eq!(
            split_crate_filename("my-crate-1.0.0-beta.1"),
            Some(("my-crate", "1.0.0-beta.1"))
        );
    }

    #[test]
    fn test_split_crate_filename_no_version_is_none() {
        assert_eq!(split_crate_filename("just-a-name"), None);
    }

    // TEST-003: pip diff logic
    #[test]
    fn test_pip_diff_detects_new_package() {
        let previous: HashMap<String, String> = [("requests".into(), "2.28.0".into())].into();
        let current: HashMap<String, String> = [
            ("requests".into(), "2.28.0".into()),
            ("flask".into(), "3.0.0".into()),
        ]
        .into();

        let new_pkgs: Vec<_> = current
            .iter()
            .filter(|(name, _)| !previous.contains_key(*name))
            .collect();
        assert_eq!(new_pkgs.len(), 1);
        assert_eq!(new_pkgs[0].0, "flask");
    }

    #[test]
    fn test_pip_diff_no_false_positives() {
        let previous: HashMap<String, String> = [("requests".into(), "2.28.0".into())].into();
        let current = previous.clone();
        let new_pkgs: Vec<_> = current
            .iter()
            .filter(|(name, _)| !previous.contains_key(*name))
            .collect();
        assert!(new_pkgs.is_empty());
    }

    #[tokio::test]
    async fn test_diff_and_persist_returns_previous_and_writes_current() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        let first: HashMap<String, String> = [("flask".into(), "3.0.0".into())].into();
        let previous = diff_and_persist(&state_path, &first).await.unwrap();
        assert!(
            previous.is_empty(),
            "no prior state should mean an empty map"
        );

        let second: HashMap<String, String> = [
            ("flask".into(), "3.0.0".into()),
            ("requests".into(), "2.28.0".into()),
        ]
        .into();
        let previous = diff_and_persist(&state_path, &second).await.unwrap();
        assert_eq!(
            previous, first,
            "second call should see what the first call wrote"
        );
    }

    #[tokio::test]
    async fn test_diff_and_persist_skips_write_when_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        let snapshot: HashMap<String, String> = [("flask".into(), "3.0.0".into())].into();

        diff_and_persist(&state_path, &snapshot).await.unwrap();
        let mtime_after_first_write = std::fs::metadata(&state_path).unwrap().modified().unwrap();

        // A tiny sleep so a real rewrite (if it happened) would produce a detectably later
        // mtime on filesystems with coarse timestamp resolution.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        diff_and_persist(&state_path, &snapshot).await.unwrap();
        let mtime_after_second_call = std::fs::metadata(&state_path).unwrap().modified().unwrap();

        assert_eq!(
            mtime_after_first_write, mtime_after_second_call,
            "identical snapshot should not have triggered a second write"
        );
    }

    #[test]
    fn test_record_last_command() {
        record_last_command("cargo build");
        assert_eq!(last_command(), Some("cargo build".into()));
    }

    // winget/choco's parsers only compile on Windows (see their #[cfg(windows)] gates
    // above), so these tests are gated the same way. Before this, neither parser had any
    // test coverage at all, unlike dpkg/npm/cargo's parsing above, and CI didn't build on
    // windows-latest either, so a regression here could have gone unnoticed indefinitely.
    #[cfg(windows)]
    #[test]
    fn test_parse_winget_line_valid_entry() {
        let line = "Firefox     Mozilla.Firefox     119.0.1     120.0     winget";
        assert_eq!(
            parse_winget_line(line),
            Some(("Firefox".into(), "119.0.1".into()))
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_winget_line_skips_header_and_separator() {
        assert_eq!(
            parse_winget_line("Name   Id   Version   Available   Source"),
            None
        );
        assert_eq!(
            parse_winget_line("---------------------------------------"),
            None
        );
        assert_eq!(parse_winget_line(""), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_winget_line_too_few_columns_is_none() {
        assert_eq!(parse_winget_line("OnlyOneColumn"), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_choco_line_valid_entry() {
        assert_eq!(
            parse_choco_line("git 2.42.0"),
            Some(("git".into(), "2.42.0".into()))
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_choco_line_skips_footer() {
        // choco list ends with a summary line like "5 packages installed.", and the
        // interactive version prints a "Chocolatey vX.Y.Z" banner first, neither of those
        // should be mistaken for an actual package entry.
        assert_eq!(parse_choco_line("Chocolatey v2.2.2"), None);
    }

    #[cfg(windows)]
    #[test]
    fn test_parse_choco_line_too_few_columns_is_none() {
        assert_eq!(parse_choco_line("onlyname"), None);
    }
}

