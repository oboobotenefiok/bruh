//! CONFIG-001: env var overrides.  CONFIG-002: validation.
//! POLISH-004: Windows-compatible paths via cfg! guards.
// This is the single source of truth for every tunable setting in bruh, plus the platform
// path resolution everything else in the project leans on (data_dir, config_dir, and so
// on). I wanted one obvious place to look when someone asks "where does bruh store X" or
// "how do I change Y", rather than scattering path logic and settings across every file
// that happens to need them.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

// ── Platform-aware path helpers ───────────────────────────────────────────────
// Windows, macOS, and Linux all have different conventions for "where does an app store
// its stuff", so every path helper here branches on cfg(windows) rather than assuming
// Unix-style paths everywhere. This was one of the POLISH-004 fixes, originally I only
// tested this on my own machine (Linux) and it just silently broke on Windows testers.

// Figures out the user's home directory. On Windows we check USERPROFILE first since
// that's the modern standard env var, falling back to HOMEPATH, and if somehow neither is
// set we fall back to a public folder rather than panicking, better a wrong-but-valid path
// than a crash. Unix just reads $HOME the normal way.
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        std::env::var("USERPROFILE")
            .or_else(|_| std::env::var("HOMEPATH"))
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("C:\\Users\\Public"))
    }
    #[cfg(not(windows))]
    {
        std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// Application data directory (writable, not user-visible config).
// This is where the daemon writes its actual working state: cursors, state snapshots for
// package managers, the offline buffer, health.json, all the stuff the user isn't expected
// to hand-edit. Deliberately kept separate from config_dir below, which holds the stuff a
// human might actually want to open and tweak.
pub fn data_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join("AppData").join("Local"));
        Ok(base.join("bruh"))
    }
    #[cfg(not(windows))]
    {
        Ok(home_dir().join(".local/share/bruh"))
    }
}

/// User-facing configuration directory.
// Holds config.json (the human-editable settings) and learned_managers.json (the discovery
// cache). This is the one you'd point a text editor at if you wanted to hand-tweak
// something rather than going through `bruh config set`.
pub fn config_dir() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        let base = std::env::var("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join("AppData").join("Roaming"));
        Ok(base.join("bruh"))
    }
    #[cfg(not(windows))]
    {
        Ok(home_dir().join(".config/bruh"))
    }
}

// ── Config struct ─────────────────────────────────────────────────────────────
// Every field here maps 1:1 to something the daemon or CLI reads at some point. I kept it
// flat (no nested structs) on purpose, it makes get_value/set_value below dramatically
// simpler to write since there's no path traversal to worry about, just a match on a
// string key.

// CONFIG-004: `#[serde(default)]` here means any field missing from an on-disk config.json
// gets filled in from Config::default() below instead of making the whole file fail to
// parse. This matters more than it looks like it should, every time we add a new field
// (like the three LLM keys just below), anyone with a config.json saved from before that
// change has a file on disk that's missing it. Without this attribute, serde treats a
// missing field as a hard error, "missing field `gemini_api_key`", and `bruh providers`
// (or literally any command, since they all call Config::load()) refuses to start at all
// until you manually edit or delete your config file. With it, an old config just quietly
// gets the new field's default value the first time it's loaded, and gets written back out
// complete with defaults the next time you run `bruh config set` on anything.
#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(default)]
pub struct Config {
    pub cognee_api_key: String,
    pub cognee_api_url: String,
    // CONFIG-003: LLM provider keys, settable via `bruh config set` instead of only
    // env vars. These stay empty by default on purpose, an empty string here just means
    // "fall back to the provider's native env var" (GOOGLE_AI_API_KEY, GROQ_API_KEY,
    // ANTHROPIC_API_KEY), see resolved_gemini_key() and friends below. That way nobody
    // who already has these env vars set loses anything by upgrading, config just gives
    // a second, more discoverable way to set them.
    pub gemini_api_key: String,
    pub groq_api_key: String,
    pub claude_api_key: String,
    pub llm_priority: Vec<String>,
    pub discovery_enabled: bool,
    pub discovery_rate_limit_seconds: u64,
    pub poll_interval_seconds: u64,
    pub batch_flush_interval_seconds: u64,
    pub offline_buffer_path: PathBuf,
    pub excluded_commands: Vec<String>,
    pub max_buffer_size: usize,
    pub daemon_log_level: String,
}

// These are the values a fresh install starts with before `bruh init` or any manual editing
// happens. Worth calling out excluded_commands specifically: those regex patterns are the
// default privacy net, catching destructive commands (rm -rf, dd, mkfs) and anything that
// looks like it's setting a secret (export/set FOO_KEY=..., FOO_SECRET=..., etc) so we
// don't accidentally remember someone's API key just because they exported it in their
// shell. Both bash-style `export` and Windows-style `set` variants are covered.
impl Default for Config {
    fn default() -> Self {
        let buf_path = data_dir()
            .map(|d| d.join("buffer.ndjson"))
            .unwrap_or_else(|_| PathBuf::from("buffer.ndjson"));
        Self {
            cognee_api_key: String::new(),
            cognee_api_url: "https://api.cognee.ai".into(),
            gemini_api_key: String::new(),
            groq_api_key: String::new(),
            claude_api_key: String::new(),
            llm_priority: vec!["gemini".into(), "groq".into(), "claude".into()],
            discovery_enabled: true,
            discovery_rate_limit_seconds: 300,
            poll_interval_seconds: 30,
            // COGNEE-019/COGNEE-021: this used to be 60. Now that the daemon's flush goes
            // through /api/v1/add instead of /api/v1/remember (see ingest.rs), a flush
            // itself is fast again, there's no cognify riding along with it anymore. So
            // this bump isn't about giving a slow request more room, it's about giving the
            // *separate* periodic improve() trigger (see daemon/mod.rs) breathing room
            // between flushes, so we're not kicking off graph-build attempts on top of
            // graph-build attempts. 240s (4 minutes) is a reasonable middle ground for a
            // background daemon, still fresh enough for `bruh explain`/`bruh stats` to feel
            // current, without hammering Cognee on every single tick.
            batch_flush_interval_seconds: 240,
            offline_buffer_path: buf_path,
            excluded_commands: vec![
                "rm -rf".into(),
                "dd ".into(),
                "mkfs".into(),
                "sudo shutdown".into(),
                "history".into(),
                "export.*KEY".into(),
                "export.*SECRET".into(),
                "export.*PASSWORD".into(),
                "export.*TOKEN".into(),
                // Windows-specific secrets
                "set.*KEY".into(),
                "set.*SECRET".into(),
                "set.*PASSWORD".into(),
            ],
            max_buffer_size: 10_000,
            daemon_log_level: "info".into(),
        }
    }
}

impl Config {
    // The main entry point basically everything else in the codebase calls. Loads whatever
    // is saved on disk (or defaults if nothing's saved yet), then layers env var overrides
    // on top so BRUH_* vars always win, useful for one-off overrides without touching the
    // saved config file, like in CI or a quick debugging session.
    pub fn load() -> Result<Self> {
        let mut cfg = Self::load_from_disk()?;
        cfg.apply_env_overrides();
        Ok(cfg)
    }

    fn load_from_disk() -> Result<Self> {
        let path = Self::config_path()?;
        if !path.exists() {
            return Ok(Self::default());
        }
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read config: {:?}", path))?;
        serde_json::from_str(&raw).with_context(|| format!("Failed to parse config: {:?}", path))
    }

    /// CONFIG-001: BRUH_* env vars override all config fields.
    // Each override here is deliberately independent, missing or malformed env vars just
    // get skipped (via if let Ok / if let Ok(n) = v.parse()) rather than erroring, so a typo
    // in one env var doesn't stop the rest of the config from loading. Some vars also check
    // a couple of alternate names (BRUH_COGNEE_API_KEY or plain COGNEE_API_KEY) since I
    // figured people might already have COGNEE_API_KEY set from using Cognee directly.
    fn apply_env_overrides(&mut self) {
        if let Ok(v) =
            std::env::var("BRUH_COGNEE_API_KEY").or_else(|_| std::env::var("COGNEE_API_KEY"))
        {
            self.cognee_api_key = v;
        }
        if let Ok(v) = std::env::var("BRUH_COGNEE_API_URL")
            .or_else(|_| std::env::var("COGNEE_API_URL"))
            .or_else(|_| std::env::var("COGNEE_BASE_URL"))
        {
            self.cognee_api_url = v;
        }
        if let Ok(v) = std::env::var("BRUH_POLL_INTERVAL") {
            if let Ok(n) = v.parse() {
                self.poll_interval_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("BRUH_FLUSH_INTERVAL") {
            if let Ok(n) = v.parse() {
                self.batch_flush_interval_seconds = n;
            }
        }
        if let Ok(v) = std::env::var("BRUH_DISCOVERY_ENABLED") {
            self.discovery_enabled = !matches!(v.to_lowercase().as_str(), "0" | "false" | "no");
        }
        if let Ok(v) = std::env::var("BRUH_MAX_BUFFER_SIZE") {
            if let Ok(n) = v.parse() {
                self.max_buffer_size = n;
            }
        }
        if let Ok(v) = std::env::var("BRUH_LOG_LEVEL") {
            self.daemon_log_level = v;
        }
    }

    /// CONFIG-002: validate config values.
    // Catches the config states that would make the daemon misbehave in confusing ways
    // rather than failing clearly. The flush-vs-poll interval check specifically exists
    // because if flush happened more often than poll, we'd be trying to send batches that
    // are usually empty, technically harmless but wasteful and a sign the user probably
    // mistyped something.
    pub fn validate(&self) -> Result<()> {
        if self.poll_interval_seconds == 0 {
            anyhow::bail!("poll_interval_seconds must be > 0");
        }
        if self.batch_flush_interval_seconds == 0 {
            anyhow::bail!("batch_flush_interval_seconds must be > 0");
        }
        if self.batch_flush_interval_seconds < self.poll_interval_seconds {
            anyhow::bail!(
                "batch_flush_interval_seconds ({}) must be >= poll_interval_seconds ({})",
                self.batch_flush_interval_seconds,
                self.poll_interval_seconds
            );
        }
        if self.max_buffer_size == 0 {
            anyhow::bail!("max_buffer_size must be > 0");
        }
        Ok(())
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path()?;
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)
                .with_context(|| format!("Failed to create config dir: {:?}", p))?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("Failed to write config: {:?}", path))
    }

    // These path helpers are what the rest of the codebase actually calls rather than
    // reaching for data_dir()/config_dir() directly everywhere, keeps every consumer
    // agnostic to the exact directory layout, if I ever want to reorganize where things
    // live I only need to change it here.
    pub fn config_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.json"))
    }
    pub fn learned_managers_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("learned_managers.json"))
    }
    pub fn data_dir() -> Result<PathBuf> {
        data_dir()
    }
    pub fn health_file_path() -> Result<PathBuf> {
        Ok(data_dir()?.join("health.json"))
    }

    /// Git events drop-file used on Windows (and as fallback on Unix).
    pub fn git_events_path() -> Result<PathBuf> {
        Ok(data_dir()?.join("git_events.ndjson"))
    }

    // CONFIG-003: these are what the discovery providers actually call now instead of
    // reaching for env::var() directly. Config wins if it's set, since that's the more
    // explicit "the user told us on purpose" source, the env var is just the fallback for
    // anyone who set it up before this existed (or who just prefers env vars, CI runners
    // for example). Keeping one small function per provider instead of one generic
    // "resolve(name)" helper because the env var name differs per provider and doesn't
    // follow a pattern I can derive from the config field name.
    pub fn resolved_gemini_key(&self) -> Option<String> {
        if !self.gemini_api_key.is_empty() {
            Some(self.gemini_api_key.clone())
        } else {
            std::env::var("GOOGLE_AI_API_KEY").ok()
        }
    }
    pub fn resolved_groq_key(&self) -> Option<String> {
        if !self.groq_api_key.is_empty() {
            Some(self.groq_api_key.clone())
        } else {
            std::env::var("GROQ_API_KEY").ok()
        }
    }
    pub fn resolved_claude_key(&self) -> Option<String> {
        if !self.claude_api_key.is_empty() {
            Some(self.claude_api_key.clone())
        } else {
            std::env::var("ANTHROPIC_API_KEY").ok()
        }
    }

    // Powers `bruh config get <key>` and `bruh config list`. The API key gets masked
    // deliberately, we never want to print the actual secret to a terminal that might be
    // screen-shared or logged, "<hidden>" tells the user it's set without leaking it.
    pub fn get_value(&self, key: &str) -> Option<String> {
        match key {
            "cognee_api_key" => Some(if self.cognee_api_key.is_empty() {
                "<not set>".into()
            } else {
                "<hidden>".into()
            }),
            "cognee_api_url" => Some(self.cognee_api_url.clone()),
            // Same masking treatment as cognee_api_key, and for the same reason, these
            // are secrets too and shouldn't show up in a shared terminal or bug report.
            "gemini_api_key" => Some(if self.gemini_api_key.is_empty() {
                "<not set>".into()
            } else {
                "<hidden>".into()
            }),
            "groq_api_key" => Some(if self.groq_api_key.is_empty() {
                "<not set>".into()
            } else {
                "<hidden>".into()
            }),
            "claude_api_key" => Some(if self.claude_api_key.is_empty() {
                "<not set>".into()
            } else {
                "<hidden>".into()
            }),
            "llm_priority" => Some(self.llm_priority.join(",")),
            "discovery_enabled" => Some(self.discovery_enabled.to_string()),
            "discovery_rate_limit_seconds" => Some(self.discovery_rate_limit_seconds.to_string()),
            "poll_interval_seconds" => Some(self.poll_interval_seconds.to_string()),
            "batch_flush_interval_seconds" => Some(self.batch_flush_interval_seconds.to_string()),
            "max_buffer_size" => Some(self.max_buffer_size.to_string()),
            "daemon_log_level" => Some(self.daemon_log_level.clone()),
            "offline_buffer_path" => Some(self.offline_buffer_path.to_string_lossy().to_string()),
            // Showing the count rather than every regex pattern verbatim, `bruh config list`
            // is meant to be a quick scan, not a dump. Someone who wants to see the actual
            // patterns can open the config file directly (see the path list prints below).
            "excluded_commands" => Some(format!(
                "{} pattern(s) configured",
                self.excluded_commands.len()
            )),
            _ => None,
        }
    }

    // Powers `bruh config set <key> <value>`. Every branch parses the string value into
    // whatever type the field actually needs, and every parse failure gets wrapped with a
    // helpful "Invalid number: <value>" style message via .with_context() rather than a
    // bare parse error. Runs validate() at the end so a bad set can't leave the in-memory
    // config in a state that would break the daemon, the caller (cli/config_cli.rs) is
    // expected to bail out and not save if this returns an error.
    pub fn set_value(&mut self, key: &str, value: &str) -> Result<()> {
        match key {
            "cognee_api_key" => {
                self.cognee_api_key = value.into();
            }
            "cognee_api_url" => {
                self.cognee_api_url = value.into();
            }
            "gemini_api_key" => {
                self.gemini_api_key = value.into();
            }
            "groq_api_key" => {
                self.groq_api_key = value.into();
            }
            "claude_api_key" => {
                self.claude_api_key = value.into();
            }
            "llm_priority" => {
                self.llm_priority = value.split(',').map(|s| s.trim().into()).collect();
            }
            "discovery_enabled" => {
                self.discovery_enabled =
                    matches!(value.to_lowercase().as_str(), "true" | "1" | "yes");
            }
            "discovery_rate_limit_seconds" => {
                self.discovery_rate_limit_seconds = value
                    .parse()
                    .with_context(|| format!("Invalid number: {}", value))?;
            }
            "poll_interval_seconds" => {
                self.poll_interval_seconds = value
                    .parse()
                    .with_context(|| format!("Invalid number: {}", value))?;
            }
            "batch_flush_interval_seconds" => {
                self.batch_flush_interval_seconds = value
                    .parse()
                    .with_context(|| format!("Invalid number: {}", value))?;
            }
            "max_buffer_size" => {
                self.max_buffer_size = value
                    .parse()
                    .with_context(|| format!("Invalid number: {}", value))?;
            }
            "daemon_log_level" => {
                self.daemon_log_level = value.into();
            }
            _ => anyhow::bail!("Unknown config key '{}'. Run 'bruh config list'.", key),
        }
        self.validate()
    }
}
