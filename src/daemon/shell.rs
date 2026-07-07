//! SHELL-001: bash + PowerShell.  SHELL-002: multi-line.  SHELL-003: regex exclusion.
//! SHELL-004: zsh timestamps.  SHELL-005: cd-tracking for working directory.
//! POL-004: Windows PowerShell history.  POLISH-005: byte-offset seek.
// This is the file that watches your shell history and turns raw history lines into
// ShellCommandEvent records. It's probably the trickiest poller in the daemon because shell
// history formats are genuinely messy: zsh's extended history has timestamps and elapsed
// time baked into each line, bash's is just plain commands with no metadata, and
// PowerShell's is different again. So a decent chunk of this file is just format parsing.

use crate::cli::{home_dir, Config};
use crate::events::{classify_error, command_hash, Event, ShellCommandEvent};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Utc};
use log::debug;
use regex::Regex;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::PathBuf;
use std::sync::OnceLock;

// SHELL-003: compiled once and reused across every poll tick rather than recompiling regex
// patterns from the config on every single call, regex compilation isn't free and this runs
// on a tight polling loop.
static EXCLUSION_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

// Bad regex patterns in the user's config just get silently dropped here (filter_map with
// .ok()) rather than erroring the whole daemon out over a typo in an exclusion rule.
fn build_exclusion_patterns(excluded: &[String]) -> Vec<Regex> {
    excluded.iter().filter_map(|p| Regex::new(p).ok()).collect()
}

fn is_excluded(command: &str, patterns: &[Regex]) -> bool {
    patterns.iter().any(|r| r.is_match(command))
}

// The main entry point called once per poll tick from daemon/mod.rs. Walks whatever shell
// history files exist for the current platform, reads only the NEW bytes since last time
// (via the byte-offset cursor, see POLISH-005 below), parses those bytes into structured
// entries, filters out anything matching an exclusion pattern (so secrets typed as env vars
// don't end up remembered forever), and turns what's left into events.
pub async fn poll_shell_history(config: &Config) -> Result<Vec<Event>> {
    let patterns =
        EXCLUSION_PATTERNS.get_or_init(|| build_exclusion_patterns(&config.excluded_commands));

    let mut events = Vec::new();
    let home = home_dir();
    let data_dir = Config::data_dir()?;
    std::fs::create_dir_all(&data_dir)?;

    // Platform-specific history files
    // Windows doesn't really have zsh-style history, PowerShell keeps its own
    // ConsoleHost_history.txt under AppData, so on Windows we look there plus Git Bash's
    // .bash_history as a bonus in case that's installed too. Everywhere else, zsh and bash
    // are the two we care about.
    let history_sources: Vec<(PathBuf, HistoryFormat)> = {
        #[cfg(windows)]
        {
            let ps_history = std::env::var("APPDATA")
                .map(|d| PathBuf::from(d)
                    .join("Microsoft/Windows/PowerShell/PSReadLine/ConsoleHost_history.txt"))
                .unwrap_or_else(|_| home.join("AppData/Roaming/Microsoft/Windows/PowerShell/PSReadLine/ConsoleHost_history.txt"));
            vec![
                (ps_history, HistoryFormat::Plain),
                (home.join(".bash_history"), HistoryFormat::Plain), // Git Bash
            ]
        }
        #[cfg(not(windows))]
        {
            vec![
                (home.join(".zsh_history"), HistoryFormat::Zsh),
                (home.join(".bash_history"), HistoryFormat::Plain),
            ]
        }
    };

    for (history_path, format) in &history_sources {
        if !history_path.exists() {
            continue;
        }

        let cursor_name = format!(
            "{}.cursor",
            history_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        );
        let cursor_path = data_dir.join(cursor_name);
        let byte_offset = read_byte_cursor(&cursor_path);

        let (content, new_offset) = read_new_content(history_path, byte_offset)?;
        if content.is_empty() {
            write_byte_cursor(&cursor_path, new_offset)?;
            continue;
        }

        // Parse the new content into entries
        let mut entries = match format {
            HistoryFormat::Zsh => parse_zsh_history_str(&content),
            HistoryFormat::Plain => parse_plain_history_str(&content),
        };

        // SHELL-005: reconstruct working directories from cd commands
        reconstruct_directories(&mut entries);

        for entry in &entries {
            if entry.command.is_empty() {
                continue;
            }
            if is_excluded(&entry.command, patterns) {
                continue;
            }

            events.push(Event::ShellCommand(ShellCommandEvent {
                timestamp: entry.timestamp,
                directory: entry.directory.clone(),
                command: entry.command.clone(),
                exit_code: entry.exit_code,
                output: entry.stderr.clone(),
                duration_ms: entry.duration_ms,
                session_id: None,
                command_hash: Some(command_hash(&entry.command)),
                error_type: entry.stderr.as_deref().and_then(classify_error),
            }));
            debug!("Shell event: {}", &entry.command);
        }

        write_byte_cursor(&cursor_path, new_offset)?;
    }

    Ok(events)
}

// ── POLISH-005: seek-based reading ──
// Reading the whole history file on every poll tick and re-parsing all of it would get
// slower and slower as history grows over weeks of use, so instead we remember exactly how
// many bytes we'd already read (the cursor) and seek straight there, only reading and
// parsing what's new. Much cheaper, and it means poll ticks stay fast even with a huge
// history file.

/// Read only the bytes after `byte_offset`.  Returns (new_content, new_offset).
fn read_new_content(path: &PathBuf, byte_offset: u64) -> Result<(String, u64)> {
    let metadata = std::fs::metadata(path).with_context(|| format!("Cannot stat {:?}", path))?;
    let file_len = metadata.len();

    // File was truncated (user cleared history), so reset
    // If the file got shorter than our cursor (someone ran `history -c` or manually
    // cleared it), our old offset is now past the end of the file, which would make seek
    // fail or behave weirdly. We just reset the cursor to the current file length and treat
    // this poll as "nothing new" rather than trying to recover the old position.
    if byte_offset > file_len {
        return Ok((String::new(), file_len));
    }

    let mut file = File::open(path)?;
    if byte_offset > 0 {
        file.seek(SeekFrom::Start(byte_offset))?;
    }

    let mut buf = String::new();
    file.read_to_string(&mut buf)?;
    Ok((buf, file_len))
}

fn read_byte_cursor(path: &PathBuf) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

fn write_byte_cursor(path: &PathBuf, offset: u64) -> Result<()> {
    std::fs::write(path, offset.to_string())?;
    Ok(())
}

// ── History formats ──────

enum HistoryFormat {
    Zsh,
    Plain,
}

// A normalized in-between representation both parsers produce, before we turn entries into
// the actual ShellCommandEvent the rest of the daemon deals with. Having this intermediate
// struct made testing the parsers in isolation a lot easier too (see the tests module below).
#[derive(Debug)]
struct HistoryEntry {
    timestamp: DateTime<Utc>,
    command: String,
    exit_code: Option<i32>,
    duration_ms: Option<u64>,
    directory: String,
    stderr: Option<String>,
}

/// SHELL-004 + SHELL-002: zsh extended history with multi-line support.
// zsh's "extended history" format (setopt EXTENDED_HISTORY) prefixes each entry with
// ": <epoch>:<elapsed>;<command>". A plain line without that prefix can show up too
// (older entries, or history written before extended history was turned on), so we handle
// both. Multi-line commands (SHELL-002, think a command ending in a trailing backslash)
// span several raw lines in the file but should become ONE HistoryEntry, so we peek ahead
// and keep swallowing continuation lines until we hit the next ": " header or a blank line.
fn parse_zsh_history_str(content: &str) -> Vec<HistoryEntry> {
    let mut entries = Vec::new();
    let mut lines = content.lines().peekable();

    while let Some(line) = lines.next() {
        if line.trim().is_empty() {
            continue;
        }

        if line.starts_with(": ") {
            if let Some(semi) = line.find(';') {
                let header = &line[2..semi];
                let cmd_part = &line[semi + 1..];

                let (ts, elapsed) = parse_zsh_header(header);

                // SHELL-002: collect continuation lines
                let mut full_cmd = cmd_part.to_string();
                while let Some(next) = lines.peek() {
                    if next.starts_with(": ") || next.trim().is_empty() {
                        break;
                    }
                    full_cmd.push('\n');
                    full_cmd.push_str(next);
                    lines.next();
                }

                entries.push(HistoryEntry {
                    timestamp: ts,
                    command: full_cmd.trim().to_string(),
                    exit_code: None,
                    duration_ms: elapsed.map(|e| e * 1000),
                    directory: current_dir_guess(),
                    stderr: None,
                });
            }
        } else {
            // No ": " prefix, treat the whole line as a bare command with no timestamp
            // metadata available, best we can do is stamp it with "now."
            entries.push(HistoryEntry {
                timestamp: Utc::now(),
                command: line.trim().to_string(),
                exit_code: None,
                duration_ms: None,
                directory: current_dir_guess(),
                stderr: None,
            });
        }
    }
    entries
}

// The header portion is "<epoch>:<elapsed>", so we split on the first colon, parse the
// epoch seconds into a proper DateTime<Utc>, and grab elapsed seconds if present. Falls
// back to Utc::now() if the epoch doesn't parse for whatever reason, better an approximate
// timestamp than no event at all.
fn parse_zsh_header(header: &str) -> (DateTime<Utc>, Option<u64>) {
    let mut parts = header.splitn(2, ':');
    let epoch_str = parts.next().unwrap_or("").trim();
    let elapsed_str = parts.next().unwrap_or("").trim();
    let ts = epoch_str
        .parse::<i64>()
        .ok()
        .and_then(|e| Utc.timestamp_opt(e, 0).single())
        .unwrap_or_else(Utc::now);
    (ts, elapsed_str.parse::<u64>().ok())
}

/// SHELL-001 / POLISH-004: plain format, covers bash and PowerShell.
// Both bash's .bash_history and PowerShell's ConsoleHost_history.txt are just one command
// per line with zero metadata, no timestamp, no exit code, nothing. So this parser is much
// simpler than the zsh one, we just filter out blank lines and comment lines (bash history
// can have '#' timestamp comments if HISTTIMEFORMAT is set, we skip those rather than
// trying to parse them as commands).
fn parse_plain_history_str(content: &str) -> Vec<HistoryEntry> {
    content
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|line| HistoryEntry {
            timestamp: Utc::now(),
            command: line.trim().to_string(),
            exit_code: None,
            duration_ms: None,
            directory: current_dir_guess(),
            stderr: None,
        })
        .collect()
}

fn current_dir_guess() -> String {
    std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "~".to_string())
}

// ── SHELL-005: working directory tracking via cd command sequence ─────────────
// History files don't record which directory each command ran in, only the command text
// itself. So to give every event a meaningful `directory` field, we replay the sequence of
// commands starting from wherever we currently are, and whenever we spot a `cd` command we
// update our tracked "current" directory accordingly. It's an approximation (if the daemon
// wasn't running continuously since the very first command in history, our starting point
// is a guess) but it's good enough to be genuinely useful for recall().

fn reconstruct_directories(entries: &mut Vec<HistoryEntry>) {
    let home = home_dir();
    let mut current = std::env::current_dir().unwrap_or_else(|_| home.clone());

    for entry in entries.iter_mut() {
        entry.directory = current.to_string_lossy().to_string();

        if let Some(new_dir) = extract_cd_target(&entry.command, &current, &home) {
            current = new_dir;
        }
    }
}

// Handles the handful of cd forms people actually type: bare `cd` (goes home), `cd ~`,
// absolute paths (both Unix `/foo` and Windows `C:\foo`), home-relative `~/foo`, `cd ..`,
// and plain relative paths. Deliberately does NOT try to handle `cd -` (back to previous
// dir) since tracking that would need its own directory history stack, felt like overkill
// for what this feature needs to deliver.
fn extract_cd_target(cmd: &str, current: &PathBuf, home: &PathBuf) -> Option<PathBuf> {
    let cmd = cmd.trim();
    // Match bare `cd` or `cd <path>`; skip compound commands
    if cmd != "cd" && !cmd.starts_with("cd ") {
        return None;
    }

    let target = if cmd == "cd" { "" } else { cmd[3..].trim() };

    if target.is_empty() || target == "~" {
        return Some(home.clone());
    }

    // Windows-style absolute path
    if cfg!(windows) && target.len() >= 2 && target.chars().nth(1) == Some(':') {
        return Some(PathBuf::from(target));
    }
    // Unix absolute
    if target.starts_with('/') {
        return Some(PathBuf::from(target));
    }
    // Home-relative
    if target.starts_with("~/") {
        return Some(home.join(&target[2..]));
    }
    // Parent
    if target == ".." {
        return Some(current.parent().unwrap_or(current).to_path_buf());
    }
    // Relative
    Some(current.join(target))
}

// ── Tests (TEST-001) ──────────────────────────────────────────────────────────
// Covers the zsh header parsing, multi-line command joining, bash's plain format, the
// exclusion regex matching, the byte-cursor persistence, and the cd-tracking logic. These
// are the parts of this file most likely to break in a subtle way if I refactor later, so
// they're worth the coverage.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_zsh_basic_entry() {
        let entries = parse_zsh_history_str(": 1700000000:0;cargo build\n");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].command, "cargo build");
    }

    #[test]
    fn test_zsh_timestamp_parsed() {
        let entries = parse_zsh_history_str(": 1700000000:0;echo hi\n");
        assert_eq!(entries[0].timestamp.timestamp(), 1_700_000_000);
    }

    #[test]
    fn test_zsh_multiline_command() {
        let c = ": 1700000000:0;cargo build \\\n  --release\n: 1700000001:0;echo done\n";
        let entries = parse_zsh_history_str(c);
        assert_eq!(entries.len(), 2);
        assert!(entries[0].command.contains("--release"));
    }

    #[test]
    fn test_zsh_colons_in_command() {
        let entries = parse_zsh_history_str(": 1700000000:0;echo foo:bar:baz\n");
        assert_eq!(entries[0].command, "echo foo:bar:baz");
    }

    #[test]
    fn test_bash_plain_history() {
        let entries = parse_plain_history_str("ls -la\ngit status\n");
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].command, "ls -la");
    }

    #[test]
    fn test_exclusion_regex() {
        let patterns = build_exclusion_patterns(&["rm -rf".into(), "export.*KEY".into()]);
        assert!(is_excluded("rm -rf /tmp/foo", &patterns));
        assert!(is_excluded("export MY_API_KEY=secret", &patterns));
        assert!(!is_excluded("cargo build", &patterns));
    }

    #[test]
    fn test_byte_cursor_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("test.cursor");
        write_byte_cursor(&p, 12345).unwrap();
        assert_eq!(read_byte_cursor(&p), 12345);
    }

    #[test]
    fn test_cd_tracking_home() {
        let home = home_dir();
        let current = PathBuf::from("/some/path");
        let result = extract_cd_target("cd", &current, &home);
        assert_eq!(result, Some(home));
    }

    #[test]
    fn test_cd_tracking_relative() {
        let home = home_dir();
        let current = PathBuf::from("/home/user");
        let result = extract_cd_target("cd projects", &current, &home);
        assert_eq!(result, Some(PathBuf::from("/home/user/projects")));
    }

    #[test]
    fn test_cd_tracking_absolute() {
        let home = home_dir();
        let current = PathBuf::from("/anywhere");
        let result = extract_cd_target("cd /tmp/work", &current, &home);
        assert_eq!(result, Some(PathBuf::from("/tmp/work")));
    }

    #[test]
    fn test_non_cd_returns_none() {
        let home = home_dir();
        let current = PathBuf::from("/home/user");
        assert!(extract_cd_target("cargo build", &current, &home).is_none());
        assert!(extract_cd_target("echo cd foo", &current, &home).is_none());
    }

    #[test]
    fn test_reconstruct_directories_tracks_cd() {
        let mut entries = vec![
            HistoryEntry {
                timestamp: Utc::now(),
                command: "ls".into(),
                exit_code: None,
                duration_ms: None,
                directory: String::new(),
                stderr: None,
            },
            HistoryEntry {
                timestamp: Utc::now(),
                command: "cd /tmp".into(),
                exit_code: None,
                duration_ms: None,
                directory: String::new(),
                stderr: None,
            },
            HistoryEntry {
                timestamp: Utc::now(),
                command: "pwd".into(),
                exit_code: None,
                duration_ms: None,
                directory: String::new(),
                stderr: None,
            },
        ];
        reconstruct_directories(&mut entries);
        assert_eq!(entries[2].directory, "/tmp");
    }

    // Let's walk through the actual bug report step by step, using the real
    // poll_shell_history function instead of just reasoning about it in our heads. The
    // question we're answering is simple: does the daemon see a new command the moment
    // you type it, or only once that command has actually landed on disk? Here's the
    // catch. Bash, and zsh too unless INC_APPEND_HISTORY is turned on, only appends to
    // its history file when the shell exits or when something explicitly calls
    // history -a. It doesn't happen after every command by default. So if someone's rc
    // file hasn't been re-sourced since bruh init added the incremental-flush block,
    // their live shell is still behaving the old way, and the daemon ends up polling a
    // file that never changes. From the outside that looks like "nothing populates,"
    // even though, as this test shows, the polling code itself is doing exactly what
    // it's supposed to.
    #[test]
    fn test_unflushed_history_produces_zero_events_until_flushed() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let history_path = dir.path().join(".bash_history");
        std::fs::write(&history_path, "").unwrap(); // freshly created, nothing written yet

        let config = Config::default();
        let rt = tokio::runtime::Runtime::new().unwrap();

        // First, let's check the state everyone hits by accident: a command has been
        // typed but never flushed, because there's no history -a and the shell hasn't
        // exited. This is what a shell stuck on the old rc file looks like forever.
        let events = rt.block_on(poll_shell_history(&config)).unwrap();
        assert_eq!(events.len(), 0, "nothing on disk yet, so nothing to poll");

        // Now let's simulate the fix actually working: PROMPT_COMMAND's history -a (or
        // zsh's INC_APPEND_HISTORY) fires, and the command finally lands on disk.
        std::fs::write(&history_path, "cargo build --release\n").unwrap();
        let events = rt.block_on(poll_shell_history(&config)).unwrap();
        assert_eq!(
            events.len(),
            1,
            "poller correctly picks up newly flushed content"
        );
        if let Event::ShellCommand(sc) = &events[0] {
            assert_eq!(sc.command, "cargo build --release");
        } else {
            panic!("expected a ShellCommand event");
        }

        // One more check while we're here: poll again with nothing new appended, just to
        // make sure the byte-cursor is doing its job and we don't re-ingest the same line
        // twice.
        let events = rt.block_on(poll_shell_history(&config)).unwrap();
        assert_eq!(
            events.len(),
            0,
            "cursor should prevent re-reading the same bytes"
        );
    }

    #[test]
    fn test_classify_error_linker() {
        use crate::events::classify_error;
        assert_eq!(
            classify_error("error: linker 'cc' not found"),
            Some("linker_error".into())
        );
    }

    #[test]
    fn test_command_hash_normalises() {
        use crate::events::command_hash;
        assert_eq!(command_hash("cargo  build"), command_hash("cargo build"));
    }
}
