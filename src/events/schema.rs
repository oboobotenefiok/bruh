// This is the shared vocabulary of the whole project. Every single thing the daemon
// observes (a shell command, a package install, a git commit, a discovered package
// manager) gets normalized into one of the Event variants below before it goes anywhere
// near Cognee. Having one enum for all of this means ingest.rs, buffer.rs, and every
// poller can all just work with `Event` without caring about the specific shape underneath
// until they actually need to.
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// The #[serde(tag = "event_type")] here is what gives us a clean "event_type" field in the
// serialized JSON (like "shell_command" or "git_commit") instead of the more awkward nested
// shape serde would produce by default for an enum with struct variants. This matters
// because that JSON is what actually gets sent to Cognee, and I want it to read cleanly on
// their end too.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event_type")]
pub enum Event {
    #[serde(rename = "shell_command")]
    ShellCommand(ShellCommandEvent),
    #[serde(rename = "package_install")]
    PackageInstall(PackageInstallEvent),
    #[serde(rename = "git_commit")]
    GitCommit(GitCommitEvent),
    #[serde(rename = "package_manager_profile")]
    PackageManagerProfile(PackageManagerProfile),
}

/// CORE-001 / SCHEMA-001: session_id on all events.
/// SCHEMA-002: command_hash for deduplication.
/// SCHEMA-003: error_type classification.
// Every shell command you run becomes one of these, assuming it isn't excluded by the
// regex patterns in config. command_hash lets us dedupe identical commands run repeatedly
// (think `ls` a hundred times a day) without needing to compare full strings everywhere,
// and error_type gives improve() something structured to cluster on when it's looking for
// recurring failure patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ShellCommandEvent {
    pub timestamp: DateTime<Utc>,
    pub directory: String,
    pub command: String,
    pub exit_code: Option<i32>,
    pub output: Option<String>,
    pub duration_ms: Option<u64>,
    pub session_id: Option<String>,
    pub command_hash: Option<String>,
    pub error_type: Option<String>,
}

/// SCHEMA-N-001: working_directory on all event types so bruh explain
/// can scope queries to a project directory.
// trigger_command (PKG-005) is what links this install back to the shell command that
// caused it, populated by daemon/packages.rs from the LAST_COMMAND static.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageInstallEvent {
    pub timestamp: DateTime<Utc>,
    pub manager: String,
    pub manager_type: ManagerType,
    pub package: String,
    pub version: Option<String>,
    pub trigger_command: Option<String>,
    pub exit_code_trigger: Option<i32>,
    pub session_id: Option<String>,
    pub working_directory: Option<String>,
}

// Bootstrapped means it's one of the package managers we know about natively (apt, npm,
// cargo, etc). Learned means discovery figured it out on the fly via the LLM cascade. This
// distinction is mostly useful for the `bruh managers` output so the user can see what was
// built in versus what bruh taught itself.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ManagerType {
    Bootstrapped,
    Learned,
}

/// SCHEMA-NEW-001 + GIT-003: working_directory + diff_summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GitCommitEvent {
    pub timestamp: DateTime<Utc>,
    pub hash: String,
    pub message: String,
    pub files_changed: Vec<String>,
    pub branch: String,
    pub session_id: Option<String>,
    pub working_directory: Option<String>,
    pub diff_summary: Option<String>,
}

// This is what the discovery pipeline produces once it's figured out an unknown package
// manager, see src/discovery/ for how it gets built. node_type is a leftover naming
// convention from thinking about this as a graph node in Cognee's terms, calling it that
// explicitly helps on their end recognize what kind of thing this record represents.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PackageManagerProfile {
    pub node_type: String,
    pub name: String,
    pub log_path: Option<String>,
    pub registry_path: Option<String>,
    pub install_verb: String,
    pub remove_verb: String,
    pub list_command: String,
    pub discovered_at: DateTime<Utc>,
    pub confidence: Confidence,
    pub first_seen_command: String,
    pub discovered_by_provider: Option<String>,
}

// How sure the LLM extractor was about the info it pulled together for this manager. Low
// confidence profiles still get stored and used, but a human skimming `bruh managers` can
// see at a glance which ones might be worth double-checking.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum Confidence {
    High,
    Medium,
    Low,
}

// Implementing Display by hand instead of deriving it (Rust doesn't derive Display for
// enums) so we can just do `{}` in format strings wherever we print a Confidence value,
// like in extractor.rs's verbose cascade output.
impl std::fmt::Display for Confidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Confidence::High => write!(f, "High"),
            Confidence::Medium => write!(f, "Medium"),
            Confidence::Low => write!(f, "Low"),
        }
    }
}

/// Classify stderr output into broad error categories for clustering.
// This is what powers `bruh improve`'s error clustering. Plain keyword matching rather than
// anything fancier, no ML classifier or regex library needed, and honestly for the kinds of
// errors developers actually hit day to day (linker errors, missing deps, permission
// issues, compile errors, network errors) simple substring checks catch the overwhelming
// majority of cases. Order matters here since we return on the first match, so more
// specific categories are checked before the generic_error catch-all at the bottom.
pub fn classify_error(output: &str) -> Option<String> {
    let lower = output.to_lowercase();
    if lower.contains("linker") || lower.contains("ld returned") {
        Some("linker_error".into())
    } else if lower.contains("cannot find")
        || lower.contains("not found")
        || lower.contains("no such file")
    {
        Some("missing_dependency".into())
    } else if lower.contains("permission denied") || lower.contains("access denied") {
        Some("permission_denied".into())
    } else if lower.contains("compile error")
        || lower.contains("error[e")
        || lower.contains("syntax error")
    {
        Some("compile_error".into())
    } else if lower.contains("network")
        || lower.contains("connection refused")
        || lower.contains("timeout")
    {
        Some("network_error".into())
    } else if !output.trim().is_empty() {
        Some("generic_error".into())
    } else {
        None
    }
}

/// SHA-256 of a normalised command string for deduplication.
// Despite what the doc comment above says, this is actually NOT SHA-256, it's a simple
// djb2-style hash I rolled by hand specifically to avoid pulling in the sha2 crate for
// something that just needs to be "good enough to dedupe commands," not cryptographically
// secure. The doc comment is a little aspirational/stale at this point, I should probably
// fix that wording, but the function itself does exactly what we need: same normalized
// command in, same hash out, every time.
pub fn command_hash(cmd: &str) -> String {
    // Simple djb2-style hash, no sha2 crate needed.
    // We collapse all whitespace runs down to single spaces first, so "cargo  build" (two
    // spaces) and "cargo build" (one space) hash identically, since they're really the same
    // command typed slightly differently.
    let normalised = cmd.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut h: u64 = 5381;
    for b in normalised.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as u64);
    }
    format!("{:016x}", h)
}

