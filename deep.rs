--- ./src/discovery/providers/groq.rs ---
//! The Groq discovery backend -- second in the provider cascade, after Gemini.
//!
//! Groq is running Llama here, and their inference is genuinely fast, which matters
//! because this whole extraction step happens while the daemon is otherwise busy watching
//! for events; we don't want a slow LLM call to become a bottleneck.
use crate::{
    discovery::extractor::{build_profile, ExtractorBackend},
    events::PackageManagerProfile,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

// CONFIG-003: resolved key handed in at construction time (config value first, GROQ_API_KEY
// env var as fallback), same pattern as GeminiBackend, see gemini.rs for the longer version
// of this comment.
/// The Groq backend for package-manager discovery. See [`ExtractorBackend`].
pub struct GroqBackend {
    pub api_key: Option<String>,
}

#[async_trait]
impl ExtractorBackend for GroqBackend {
    fn name(&self) -> &'static str {
        "groq"
    }

    // Same pattern as Gemini's backend, if the key isn't set we tell the cascade to skip us.
    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        let api_key = self.api_key.clone().context(
            "No Groq key configured. Run `bruh config set groq_api_key <key>` \
             or export GROQ_API_KEY.",
        )?;

        // DISCOVERY-008: Groq/Llama works best with concise, direct prompts
        // Unlike Gemini, Llama doesn't need as much hand holding about markdown fences,
        // it tends to just answer with the JSON when you ask directly. Still, I keep the
        // "Reply ONLY with JSON" instruction because it costs nothing and helps a bit.
        let prompt = format!(
            "Extract package manager info for '{}' from these snippets. \
             Reply ONLY with JSON: {{\"install_verb\":\"...\",\"remove_verb\":\"...\",\
             \"list_command\":\"...\",\"log_path\":null,\"registry_path\":null,\
             \"confidence\":\"high|medium|low\"}}\nSnippets:\n{}",
            manager_name,
            snippets.join("\n")
        );

        // Groq mimics the OpenAI chat completions API shape, which is convenient because
        // it means this request body looks familiar if you've used OpenAI's SDK before.
        let client = super::http_client();
        let resp = client
            .post("https://api.groq.com/openai/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&serde_json::json!({
                "model": "llama-3.3-70b-versatile",
                "messages": [{"role": "user", "content": prompt}],
                "temperature": 0.1,
                "max_tokens": 256
            }))
            .send()
            .await
            .context("Groq request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "Groq HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        // OpenAI-shaped responses nest the actual reply under choices[0].message.content,
        // that's what we're pulling out here.
        let json: Value = resp.json().await.context("Groq response not JSON")?;
        let text = json["choices"][0]["message"]["content"]
            .as_str()
            .context("Groq: no content in response")?;

        build_profile(manager_name, text, "groq")
    }
}


--- ./src/discovery/providers/mod.rs ---
//! The three LLM backends the discovery system can call on to figure out an unknown package
//! manager.
//!
//! Remember the cascade design from the memory: Gemini first, then Groq, then Claude, in
//! that order of preference (mostly because of free tier limits and speed, cheapest and
//! fastest options get tried first). Each backend implements the same ExtractorBackend trait
//! from extractor.rs so the calling code in extractor.rs doesn't need to care which one
//! actually answered, it just needs something that can extract a PackageManagerProfile from
//! search snippets.

mod claude;
mod gemini;
mod groq;

// Re-exporting the three backend structs here so the rest of the discovery module can just
// do `use crate::discovery::providers::GeminiBackend` instead of drilling into each file.
pub use claude::ClaudeBackend;
pub use gemini::GeminiBackend;
pub use groq::GroqBackend;

use std::sync::OnceLock;

static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

// cognee/mod.rs made this exact fix already (see COGNEE-013 there): building a fresh
// reqwest::Client on every call means a fresh connection pool and a fresh TLS handshake
// every time, which is a real cost on a mobile connection. Discovery calls are rare enough
// (rate-limited, and usually one-shot per unknown manager) that this matters far less here
// than it did for cognee's every-30-seconds flush, but there's no reason to pay the cost at
// all when a single shared client works just as well and keeps this consistent with how the
// rest of the codebase already talks to the network.
/// The shared, process-wide `reqwest::Client` used by all three discovery backends.
pub(crate) fn http_client() -> &'static reqwest::Client {
    SHARED_CLIENT.get_or_init(reqwest::Client::new)
}


--- ./src/discovery/providers/gemini.rs ---
//! The Gemini discovery backend -- first in the provider cascade.
//!
//! It's the first one we try because Google's free tier on gemini-1.5-flash is generous
//! and the model is fast enough that it doesn't hold up the daemon for long when it hits
//! an unknown package manager.
use crate::{
    discovery::extractor::{build_profile, ExtractorBackend},
    events::PackageManagerProfile,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

// CONFIG-003: used to be a zero-sized struct that reached into env::var() directly. Now
// the cascade resolves the key once (config value first, GOOGLE_AI_API_KEY env var as
// fallback, see Config::resolved_gemini_key) and hands it to us here at construction time.
// That's the only thing that changed, is_available()/extract() below just read the field
// instead of calling env::var() themselves.
/// The Gemini backend for package-manager discovery. See [`ExtractorBackend`].
pub struct GeminiBackend {
    pub api_key: Option<String>,
}

// async_trait is one of the few "heavier" crates I let myself keep, because Rust doesn't
// support async fn in traits natively without it (well, not with dyn dispatch anyway, and
// I need dyn dispatch here since extractor.rs will hold a Vec of different backend types).
#[async_trait]
impl ExtractorBackend for GeminiBackend {
    fn name(&self) -> &'static str {
        "gemini"
    }

    // The cascade in extractor.rs calls is_available() on each backend before trying it,
    // so if the person hasn't configured a Gemini key (via `bruh config set gemini_api_key`
    // or the GOOGLE_AI_API_KEY env var) we just skip straight to Groq instead of wasting a
    // request that's guaranteed to fail.
    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        let api_key = self.api_key.clone().context(
            "No Gemini key configured. Run `bruh config set gemini_api_key <key>` \
             or export GOOGLE_AI_API_KEY.",
        )?;

        // DISCOVERY-008: Gemini needs explicit JSON output instruction
        // I learned this the hard way, if you don't explicitly say "no markdown, no code
        // fences" Gemini loves to wrap its JSON in ```json blocks which then breaks the
        // parser below. Being blunt about the format fixes it almost every time.
        let prompt = format!(
            "You are a developer tool. Extract information about the '{}' package manager from these search snippets.\n\
             Return ONLY valid JSON (no markdown, no code fences, no explanation) with exactly these fields:\n\
             {{\"install_verb\":\"...\",\"remove_verb\":\"...\",\"list_command\":\"...\",\
             \"log_path\":null,\"registry_path\":null,\"confidence\":\"high|medium|low\"}}\n\n\
             Search snippets:\n{}",
            manager_name,
            snippets.join("\n")
        );

        // Gemini's REST API takes the key as a query param rather than a header, which is a
        // bit unusual compared to the other two providers, but that's just how their API is
        // shaped so we go with it. Worth knowing if you ever add request logging here: the
        // key lives in the URL for this one provider, so never log the full request URL for
        // a Gemini call, unlike Claude/Groq's header-based auth, that would put the raw key
        // straight into daemon.log in plaintext.
        let client = super::http_client();
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:generateContent?key={}",
            api_key
        );

        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "contents": [{"parts": [{"text": prompt}]}],
                "generationConfig": {"temperature": 0.1, "maxOutputTokens": 512}
            }))
            .send()
            .await
            .context("Gemini request failed")?;

        // If the HTTP status itself isn't a success, we bail early with the body attached
        // so whoever's debugging this later (probably me) can see exactly what Google sent back.
        if !resp.status().is_success() {
            anyhow::bail!(
                "Gemini HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        // Gemini nests the actual generated text pretty deep in its response shape, hence
        // the chain of index accesses below.
        let json: Value = resp.json().await.context("Gemini response not JSON")?;
        let text = json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .context("Gemini: no text in response")?;

        build_profile(manager_name, text, "gemini")
    }
}


--- ./src/discovery/providers/claude.rs ---
//! The Claude discovery backend -- last resort in the provider cascade.
//!
//! Claude usually gives the most reliable structured output of the three, honestly, but
//! it's tried last mainly because of cost and the fact that most people won't have
//! ANTHROPIC_API_KEY set specifically for this side feature when they're already using it
//! elsewhere. If Gemini and Groq both fail or aren't configured, this is the safety net.
use crate::{
    discovery::extractor::{build_profile, ExtractorBackend},
    events::PackageManagerProfile,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

// CONFIG-003: resolved key handed in at construction time (config value first,
// ANTHROPIC_API_KEY env var as fallback), same pattern as GeminiBackend.
/// The Claude backend for package-manager discovery. See [`ExtractorBackend`].
pub struct ClaudeBackend {
    pub api_key: Option<String>,
}

#[async_trait]
impl ExtractorBackend for ClaudeBackend {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        let api_key = self.api_key.clone().context(
            "No Claude key configured. Run `bruh config set claude_api_key <key>` \
             or export ANTHROPIC_API_KEY.",
        )?;

        // DISCOVERY-008: Claude responds well to explicit structure instructions
        // I give it a literal example of the JSON shape here rather than just describing
        // the fields in prose, Claude tends to follow a shown structure very closely.
        let prompt = format!(
            "Extract package manager metadata for '{}' from the following search snippets.\n\n\
             Return ONLY a JSON object with this exact structure (no markdown, no prose):\n\
             {{\n  \"install_verb\": \"install\",\n  \"remove_verb\": \"remove\",\n\
             \"list_command\": \"list\",\n  \"log_path\": null,\n  \"registry_path\": null,\n\
             \"confidence\": \"high\"\n}}\n\n\
             Snippets:\n{}",
            manager_name,
            snippets.join("\n")
        );

        // Anthropic's Messages API wants the key in an x-api-key header (not Authorization:
        // Bearer like Groq), plus an explicit anthropic-version header. Small detail but it
        // trips people up the first time they wire this up.
        let client = super::http_client();
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 512,
                "messages": [{"role": "user", "content": prompt}]
            }))
            .send()
            .await
            .context("Claude request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "Claude HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        // Claude's response puts the text under content[0].text, a flatter shape than
        // Gemini's nesting, which is one of the small reasons I like working with it.
        let json: Value = resp.json().await.context("Claude response not JSON")?;
        let text = json["content"][0]["text"]
            .as_str()
            .context("Claude: no text in response")?;

        build_profile(manager_name, text, "claude")
    }
}


--- ./src/discovery/extractor.rs ---
//! DISCOVERY-005: extract_with_cascade_verbose for CLI --learn output.
//! DISCOVERY-008: per-provider prompt optimisation.
// This file is the brain of the cascade idea. Instead of hardcoding "call Gemini, and if
// that fails call Groq" as an if/else chain, I built a small trait (ExtractorBackend) that
// all three providers implement identically, then I loop over them in priority order. This
// means adding a fourth provider later is just: write a struct that implements the trait,
// add it to the list in ProviderCascade::from_config, done. No branching logic to touch.

use crate::{
    cli::Config,
    discovery::providers::{ClaudeBackend, GeminiBackend, GroqBackend},
    events::PackageManagerProfile,
};
use anyhow::Result;
use async_trait::async_trait;
use log::{info, warn};

// The contract every LLM backend has to satisfy. name() is just for logging so we know who
// answered, is_available() lets us skip providers whose API key isn't set without wasting a
// network round trip, and extract() is the actual work.
#[async_trait]
/// The contract every LLM discovery backend implements: identify itself, report whether
/// it's configured, and extract a package-manager profile from search snippets.
pub trait ExtractorBackend: Send + Sync {
    fn name(&self) -> &'static str;
    fn is_available(&self) -> bool;
    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile>;
}

// We store the backends as Box<dyn ExtractorBackend> because they're three different
// concrete types (GeminiBackend, GroqBackend, ClaudeBackend) but we want to treat them
// uniformly in a Vec. This is dynamic dispatch, a small runtime cost but nothing that
// matters here since we're doing network calls anyway.
/// An ordered cascade of LLM backends tried in priority order until one succeeds.
pub struct ProviderCascade {
    backends: Vec<Box<dyn ExtractorBackend>>,
}

/// The default provider order, used both as the fallback order for providers not
/// explicitly listed in the user's llm_priority config, and as the single source of truth
/// cli/providers.rs's status display reads from, so its "active provider" line can never
/// drift out of sync with what the cascade actually does.
pub const PROVIDER_ORDER: &[&str] = &["gemini", "groq", "claude"];

impl ProviderCascade {
    // Builds the cascade in the order the user configured (config.llm_priority), falling
    // back to PROVIDER_ORDER for any providers they didn't explicitly rank. I use a
    // HashMap here as scratch space just so I can remove() entries as I place them into the
    // ordered Vec, that way nothing gets duplicated and nothing gets dropped.
    /// Builds the cascade in the user's configured priority order, falling back to
    /// [`PROVIDER_ORDER`] for any providers left unranked.
    pub fn from_config(config: &Config) -> Self {
        // CONFIG-003: each backend gets its resolved key (config value first, then that
        // provider's native env var) baked in right here at construction, instead of
        // reaching into env::var() itself later. This is the one place that needs to know
        // about Config at all, the backends themselves stay dumb and just hold whatever
        // key they were handed.
        let mut map: std::collections::HashMap<&str, Box<dyn ExtractorBackend>> = [
            (
                "gemini",
                Box::new(GeminiBackend {
                    api_key: config.resolved_gemini_key(),
                }) as Box<dyn ExtractorBackend>,
            ),
            (
                "groq",
                Box::new(GroqBackend {
                    api_key: config.resolved_groq_key(),
                }) as Box<dyn ExtractorBackend>,
            ),
            (
                "claude",
                Box::new(ClaudeBackend {
                    api_key: config.resolved_claude_key(),
                }) as Box<dyn ExtractorBackend>,
            ),
        ]
        .into_iter()
        .collect();

        // First pass: pull backends out in the exact order the config asks for.
        let mut backends: Vec<Box<dyn ExtractorBackend>> = config
            .llm_priority
            .iter()
            .filter_map(|name| map.remove(name.as_str()))
            .collect();

        // Append any providers not in the priority list, walking PROVIDER_ORDER rather than
        // just draining the HashMap directly. HashMap's iteration order isn't guaranteed to
        // stay the same between runs, so without this, the fallback order for providers the
        // user didn't rank (and therefore which one effectively goes first when llm_priority
        // doesn't mention everyone) could silently change from one run of bruh to the next
        // even with identical config. Walking PROVIDER_ORDER and removing from the map as we
        // go gives the same nothing-duplicated, nothing-dropped guarantee, just deterministic.
        backends.extend(PROVIDER_ORDER.iter().filter_map(|name| map.remove(name)));
        Self { backends }
    }

    // Walks the cascade in order and returns the first successful extraction. If a backend
    // isn't available (no API key) we skip it without even trying. If it IS available but
    // the call fails (rate limited, bad JSON, network hiccup, whatever) we log a warning and
    // just move on to the next one rather than giving up immediately. Only if every single
    // backend fails do we bail with an actionable error message telling the user which env
    // vars to set.
    /// Tries each available backend in order, returning the first successful extraction.
    ///
    /// # Errors
    ///
    /// Returns an error if every configured backend fails or none are available.
    pub async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        for backend in &self.backends {
            if !backend.is_available() {
                continue;
            }
            match backend.extract(manager_name, snippets).await {
                Ok(profile) => {
                    info!(
                        "[{}] extracted profile for {}",
                        backend.name(),
                        manager_name
                    );
                    return Ok(profile);
                }
                Err(e) => warn!("[{}] failed: {}. Trying next provider.", backend.name(), e),
            }
        }
        anyhow::bail!("All providers failed to extract profile for '{}'.\n\
            Configure at least one LLM provider key, either via `bruh config set gemini_api_key <key>` \
            (or groq_api_key / claude_api_key), or the matching env var \
            (GOOGLE_AI_API_KEY, GROQ_API_KEY, ANTHROPIC_API_KEY).", manager_name)
    }
}

// This is the plain entry point the silent daemon path calls (see discovery/mod.rs). It
// just loads the config fresh, builds a cascade from it, and runs it. Nothing fancy.
/// Loads the config fresh and runs [`ProviderCascade::extract`] against it; the plain,
/// non-verbose entry point the background daemon path uses.
///
/// # Errors
///
/// Returns an error if the config can't be loaded or every backend fails.
pub async fn extract_with_cascade(
    manager_name: &str,
    snippets: &[String],
) -> Result<PackageManagerProfile> {
    let config = Config::load()?;
    ProviderCascade::from_config(&config)
        .extract(manager_name, snippets)
        .await
}

/// DISCOVERY-005: same cascade with step-by-step terminal output.
// This is basically extract_with_cascade above but rewritten inline so it can print a live
// "Trying gemini... ✓" style status line for each provider as it goes. I couldn't cleanly
// reuse ProviderCascade::extract() for this because that version returns as soon as it gets
// a hit, it doesn't give me a hook to print progress per attempt, so I duplicated the loop
// here instead. A little repetitive, I know, but it keeps both code paths simple to read
// rather than one function trying to serve two very different UX needs.
/// Same cascade as [`extract_with_cascade`], but prints a live per-provider status line
/// as it goes; used by `bruh managers --learn`.
///
/// # Errors
///
/// Returns an error if every backend fails.
pub async fn extract_with_cascade_verbose(
    manager_name: &str,
    snippets: &[String],
) -> Result<PackageManagerProfile> {
    use std::io::Write;

    use crate::cli::output::{dim, green, orange};

    let config = Config::load()?;
    let cascade = ProviderCascade::from_config(&config);

    for backend in &cascade.backends {
        if !backend.is_available() {
            println!("    {} {} — key not configured", dim("–"), backend.name());
            continue;
        }
        print!("    Trying {}… ", backend.name());
        // We have to flush manually here because print! without a newline doesn't
        // automatically hit the terminal, and we want the "Trying gemini..." text visible
        // to the user before the (possibly slow) network call underneath finishes.
        std::io::stdout().flush()?;

        match backend.extract(manager_name, snippets).await {
            Ok(profile) => {
                println!("{} ({} confidence)", green("✓"), profile.confidence);
                return Ok(profile);
            }
            Err(e) => {
                println!("{} — {}", orange("✗"), dim(&e.to_string()));
            }
        }
    }

    anyhow::bail!("All LLM providers failed for '{}'", manager_name)
}

/// Parses a backend's raw LLM output into a [`PackageManagerProfile`], tolerating stray
/// prose around the JSON and falling back to sensible defaults for missing fields.
///
/// This used to be copy-pasted into gemini.rs, groq.rs, and claude.rs separately, identical
// except for which provider name showed up in the error message. Comparing all three side
// by side, there was never actually any provider-specific logic living inside this
// function, the real per-provider differences (prompt wording, markdown-fence handling,
// response nesting) all happen upstream, before build_profile is ever called. So there was
// nothing this needed abstraction to fit around, it already fit, three times over.
//
// Takes whatever text the model spat out, finds the first '{' and last '}' to strip away
// any stray prose the model added despite our instructions, then parses what's left as JSON
// and maps it onto our PackageManagerProfile struct. Falls back to sensible verb defaults
// for any field the model didn't return, better to have a guess than to fail discovery
// entirely over one missing key.
pub(crate) fn build_profile(
    name: &str,
    text: &str,
    provider: &str,
) -> Result<PackageManagerProfile> {
    use crate::events::Confidence;
    use anyhow::Context;
    use chrono::Utc;

    let start = text.find('{').unwrap_or(0);
    let end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
    let v: serde_json::Value = serde_json::from_str(&text[start..end])
        .with_context(|| format!("Failed to parse JSON from {} response", provider))?;

    Ok(PackageManagerProfile {
        node_type: "PackageManagerProfile".into(),
        name: name.to_string(),
        log_path: v["log_path"].as_str().map(|s| s.to_string()),
        registry_path: v["registry_path"].as_str().map(|s| s.to_string()),
        install_verb: v["install_verb"].as_str().unwrap_or("install").to_string(),
        remove_verb: v["remove_verb"].as_str().unwrap_or("remove").to_string(),
        list_command: v["list_command"].as_str().unwrap_or("list").to_string(),
        discovered_at: Utc::now(),
        confidence: match v["confidence"].as_str().unwrap_or("medium") {
            "high" => Confidence::High,
            "low" => Confidence::Low,
            _ => Confidence::Medium,
        },
        first_seen_command: format!("{} install <package>", name),
        discovered_by_provider: Some(provider.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_profile_parses_clean_json() {
        let text = r#"{"install_verb":"install","remove_verb":"uninstall","list_command":"list","log_path":null,"registry_path":null,"confidence":"high"}"#;
        let p = build_profile("pnpm", text, "gemini").unwrap();
        assert_eq!(p.install_verb, "install");
        assert_eq!(p.remove_verb, "uninstall");
        assert_eq!(p.discovered_by_provider, Some("gemini".into()));
    }

    #[test]
    fn build_profile_strips_surrounding_prose_and_code_fences() {
        let text = "Sure, here's the JSON:\n```json\n{\"install_verb\":\"add\",\"confidence\":\"low\"}\n```\nHope that helps!";
        let p = build_profile("pnpm", text, "claude").unwrap();
        assert_eq!(p.install_verb, "add");
        // remove_verb wasn't present, so it should fall back to the sensible default.
        assert_eq!(p.remove_verb, "remove");
    }

    #[test]
    fn build_profile_defaults_missing_fields() {
        let text = "{}";
        let p = build_profile("mystery-tool", text, "groq").unwrap();
        assert_eq!(p.install_verb, "install");
        assert_eq!(p.remove_verb, "remove");
        assert_eq!(p.list_command, "list");
    }

    #[test]
    fn build_profile_fails_on_no_json_at_all() {
        assert!(build_profile("pnpm", "no braces here", "gemini").is_err());
    }
}


--- ./src/discovery/cache.rs ---
//! DISCOVERY-007: 30-day TTL on learned managers.
// Whenever discovery successfully figures out a new package manager, we don't want to pay
// the cost (web search plus an LLM call) every single time we see it again. So we cache the
// result to disk. But package managers evolve (install syntax changes, registries move) so
// I don't want to trust a cached answer forever either, hence the 30 day expiry below.

use crate::{cli::Config, events::PackageManagerProfile};
use anyhow::Result;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CACHE_TTL_DAYS: i64 = 30;

// Wrapping the profile in its own struct instead of storing PackageManagerProfile directly
// felt like it'd give me room to add cache-specific metadata later (hit counts, last-used
// timestamp, that kind of thing) without having to touch the core event schema. Hasn't
// needed it yet, but the wrapper costs nothing to keep.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    profile: PackageManagerProfile,
}

// Reads the whole learned-managers file and returns only the entries that haven't expired.
// If the file doesn't exist yet (first run, nothing learned yet) we just hand back an empty
// map instead of erroring, since "nothing learned" is a perfectly normal state, not a
// failure.
/// Reads the learned-managers cache and returns only the entries that haven't expired
/// past the cache's TTL.
///
/// # Errors
///
/// Returns an error if the cache file exists but can't be parsed.
pub fn load_learned_managers() -> Result<HashMap<String, PackageManagerProfile>> {
    let path = Config::learned_managers_path()?;
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let content = std::fs::read_to_string(&path)?;
    // unwrap_or_default() here means a corrupted or empty cache file quietly becomes an
    // empty map rather than crashing the whole daemon over a JSON parse error. Losing the
    // cache is annoying (we'll just re-discover things) but it's recoverable, so it's not
    // worth treating as a hard failure.
    let raw: HashMap<String, CacheEntry> = serde_json::from_str(&content).unwrap_or_default();

    let cutoff = Utc::now() - Duration::days(CACHE_TTL_DAYS);

    // DISCOVERY-007: filter out expired entries
    let valid: HashMap<String, PackageManagerProfile> = raw
        .into_iter()
        .filter(|(_, entry)| entry.profile.discovered_at > cutoff)
        .map(|(k, entry)| (k, entry.profile))
        .collect();

    Ok(valid)
}

// Adds (or overwrites) one profile in the cache file. We read the existing map first,
// insert/replace the one entry, then write the whole thing back out. Not the most efficient
// approach for a huge cache, but realistically nobody's going to have thousands of package
// managers cached, so a full read-modify-write is simple and fine here.
/// Inserts or overwrites one learned profile in the on-disk cache.
///
/// # Errors
///
/// Returns an error if the cache directory or file can't be written.
pub fn save_learned_manager(profile: &PackageManagerProfile) -> Result<()> {
    let path = Config::learned_managers_path()?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }

    let mut existing: HashMap<String, CacheEntry> = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_default()
    } else {
        HashMap::new()
    };

    existing.insert(
        profile.name.clone(),
        CacheEntry {
            profile: profile.clone(),
        },
    );
    std::fs::write(&path, serde_json::to_string_pretty(&existing)?)?;
    Ok(())
}


--- ./src/discovery/register.rs ---
// src/discovery/register.rs
use crate::events::PackageManagerProfile;
use anyhow::Result;
// serde_json::json no longer needed since we're using multipart form

/// What actually happened when we tried to store a profile. This exists so callers can tell
/// "we genuinely stored this in Cognee" apart from "we skipped storing it because Cognee
/// isn't configured yet." Those two outcomes used to both come back as a plain Ok(()), which
/// meant `bruh managers --learn` could print a green checkmark for a store that never
/// actually happened, since nothing distinguished the two cases at the type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The profile made it into Cognee.
    Stored,
    /// Cognee isn't configured yet, so we skipped the remote store on purpose. Local caching
    /// (see discovery::cache) still happens regardless, this only covers the Cognee side.
    NotConfigured,
}

/// Stores a discovered profile in both Cognee (if configured) and the local cache.
///
/// # Errors
///
/// Returns an error if the store request to Cognee fails.
pub async fn store_profile(profile: &PackageManagerProfile) -> Result<StoreOutcome> {
    // If Cognee isn't configured, we don't treat that as an error, someone can perfectly
    // well use bruh's local package-manager cache before they've ever set up a Cognee key.
    // But we do need the caller to know this happened, rather than quietly reporting success
    // for a network call that was never made.
    let client = match crate::cognee::CogneeClient::from_config() {
        Ok(c) => c,
        Err(_) => return Ok(StoreOutcome::NotConfigured),
    };

    // We structure it as a multipart form. The /remember endpoint expects multipart/form-data,
    // not JSON. The datasetName field tells Cognee which dataset to store this in, using the
    // same DATASET_NAME constant that every other Cognee operation (ingest, improve, query,
    // forget) already uses to keep everything consistent.
    let documents_text = format!(
        "PACKAGE_MANAGER_PROFILE: {} install:{} list:{} confidence:{}",
        profile.name, profile.install_verb, profile.list_command, profile.confidence
    );

    // Use the existing post_multipart method on CogneeClient. It takes a closure that
    // builds the form, which is retried on each attempt if the request fails.
    client
        .post_multipart("remember", || {
            reqwest::multipart::Form::new()
                .text("datasetName", crate::cognee::DATASET_NAME.to_string())
                .text("documents", documents_text.clone())
        })
        .await?;

    Ok(StoreOutcome::Stored)
}


--- ./src/discovery/mod.rs ---
//! Self-learning discovery of unknown package managers.
//!
//! This is the discovery module's front door. Everything in here exists to answer one
//! question: "I just saw a command for a package manager I've never heard of, what is it and
//! how does it work?" That's the self-learning piece of the project I mentioned back in
//! main.rs. The submodules below each handle one step of the pipeline.
pub mod cache;
pub mod extractor;
pub mod providers;
pub mod register;

use crate::events::PackageManagerProfile;
use anyhow::{Context, Result};
use log::info;

/// The package managers bruh knows how to talk to out of the box, no discovery needed.
/// This used to exist as two separate hardcoded lists, one in daemon/discovery.rs driving
/// the actual "is this manager unknown" runtime check, and a second in cli/managers.rs
/// purely for the "always available" display line. Both had to be kept in sync by hand,
/// with nothing enforcing that they actually matched. Now there's exactly one list, and
/// both call sites read from it.
pub const BOOTSTRAPPED_MANAGERS: &[&str] = &["apt", "pip", "npm", "cargo", "pkg", "brew"];

/// DISCOVERY-009: dropped the DuckDuckGo web-search step entirely. It used to be step 1
/// of this pipeline (search.rs, since removed), but in practice the DDG Instant Answer
/// API is unreliable, it times out or comes back empty for most package manager names
/// since it's built for encyclopedia-style facts, not CLI tool trivia, so the search
/// almost always fell through to the synthetic "X is a package manager..." placeholder
/// snippet anyway. That's just extra network latency and a whole extra failure mode
/// (see the notes.txt diagnosis) for something the LLM already knows on its own. Every
/// provider in this cascade is a general-purpose model that's almost certainly seen npm,
/// cargo, pip, and their smaller cousins during training, so we just ask directly instead
/// of pretending we found "search results" first.
pub fn direct_knowledge_prompt_context(manager_name: &str) -> Vec<String> {
    vec![format!(
        "No web search was performed for this request. Answer from your own trained \
         knowledge of developer tooling and package managers. If you recognize '{0}' as a \
         real package manager or CLI tool, describe its actual install/remove/list verbs \
         and typical paths as accurately as you can recall. If you don't recognize '{0}' at \
         all, still return your best reasonable guess based on common package manager \
         conventions (most follow an `install`/`remove`/`list` verb pattern), and mark \
         confidence as \"low\" so the caller knows to double check it.",
        manager_name
    )]
}

/// Silent discovery, used by the daemon background task.
// "Silent" here means no terminal output, this runs unattended while the daemon is doing
// its normal event batching. Compare this to the --learn command path in cli/managers.rs
// which calls the verbose cascade instead so a human watching the terminal actually sees
// what's happening step by step.
//
// The pipeline is now three steps (search used to be a fourth, see
// direct_knowledge_prompt_context above for why that's gone):
//   1. feed a direct "use your own knowledge" prompt to the LLM cascade (extractor.rs)
//   2. store the resulting profile in Cognee so future queries can recall it (register.rs)
//   3. cache it locally too, so we don't have to redo this whole dance for 30 days (cache.rs)
pub async fn discover_manager(manager_name: &str) -> Result<PackageManagerProfile> {
    info!("Starting discovery for: {}", manager_name);

    let context = direct_knowledge_prompt_context(manager_name);

    let profile = extractor::extract_with_cascade(manager_name, &context)
        .await
        .context("LLM extraction failed")?;

    match register::store_profile(&profile)
        .await
        .context("Cognee store failed")?
    {
        register::StoreOutcome::Stored => {}
        // This runs unattended in the background, so nobody's watching the terminal for a
        // missed checkmark the way they would be for the explicit --learn command. A log
        // line is the only way this is ever discoverable, without it the profile silently
        // never makes it into Cognee and nothing says so anywhere.
        register::StoreOutcome::NotConfigured => {
            log::warn!(
                "Discovered '{}' but Cognee isn't configured, skipping remote store (still caching locally)",
                manager_name
            );
        }
    }

    cache::save_learned_manager(&profile)?;

    info!("Discovered: {}", manager_name);
    Ok(profile)
}


--- ./src/daemon/git.rs ---
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


--- ./src/daemon/shell.rs ---
//! SHELL-001: bash + PowerShell.  SHELL-002: multi-line.  SHELL-003: regex exclusion.
//! SHELL-004: zsh timestamps.  SHELL-005: cd-tracking for working directory.
//! POL-004: Windows PowerShell history.  POLISH-005: byte-offset seek.
// This is the file that watches your shell history and turns raw history lines into
// ShellCommandEvent records. It's probably the trickiest poller in the daemon because shell
// history formats are genuinely messy: zsh's extended history has timestamps and elapsed
// time baked into each line, bash's is just plain commands with no metadata, and
// PowerShell's is different again. So a decent chunk of this file is just format parsing.

use crate::{
    cli::{home_dir, Config},
    daemon::cursor,
    events::{classify_error, command_hash, Event, ShellCommandEvent},
};
use anyhow::Result;
use chrono::{DateTime, TimeZone, Utc};
use log::{debug, warn};
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::OnceLock,
};

// SHELL-003: compiled once and reused across every poll tick rather than recompiling regex
// patterns from the config on every single call, regex compilation isn't free and this runs
// on a tight polling loop.
static EXCLUSION_PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();

// Bad regex patterns in the user's config just get silently dropped here (filter_map with
// .ok()) rather than erroring the whole daemon out over a typo in an exclusion rule.
//
// Every pattern compiles case-insensitively on purpose. The defaults in Config (things like
// "export.*KEY") are written in the SCREAMING_SNAKE_CASE convention most people actually use
// for secrets, but a command that exports `my_api_key` or `Api_Key` instead of `API_KEY`
// deserves the exact same protection. Without case-insensitivity, a pattern written assuming
// uppercase silently misses every lowercase or mixed-case variant of the same secret.
fn build_exclusion_patterns(excluded: &[String]) -> Vec<Regex> {
    excluded
        .iter()
        .filter_map(|p| RegexBuilder::new(p).case_insensitive(true).build().ok())
        .collect()
}

// pub(crate) rather than private: cli::watch reuses this exact same check before sending
// captured error output to recall(), so there's one single definition of "does this text
// look like it might contain a secret" instead of two that could quietly drift apart.
/// Whether `command` matches one of the configured exclusion patterns (secrets, destructive
/// commands) and should be dropped rather than remembered.
pub(crate) fn is_excluded(command: &str, patterns: &[Regex]) -> bool {
    patterns.iter().any(|r| r.is_match(command))
}

// Lazily compiles (once) and hands back the exclusion patterns built from the given config.
// This is the same OnceLock the shell-history poller already uses, so calling this from
// anywhere else in the crate (cli::watch, for instance) reuses the identical compiled
// pattern set rather than paying to recompile the same regexes a second time.
/// The compiled exclusion patterns for `config`, built once and reused across the crate.
pub(crate) fn exclusion_patterns(config: &Config) -> &'static [Regex] {
    EXCLUSION_PATTERNS.get_or_init(|| build_exclusion_patterns(&config.excluded_commands))
}

// ── SHELL-006: surviving history-file truncation without duplicate re-ingestion ────────
// HISTFILESIZE trimming, `history -c`, log rotation, or someone just editing the file by
// hand all shrink a history file in place. When that happens, the byte offset our cursor
// was pointing at no longer means anything (cursor::read_new_bytes() detects this itself
// and resets to 0), which means the NEXT poll hands back the file's entire current
// content as if none of it had ever been seen, even though most of those lines were
// already ingested and sent to Cognee in a previous run. Left alone, that's a duplicate
// flood on every restart that happens to land after a trim.
//
// Byte offsets alone can't fix this: once the file has shrunk, there's no offset that
// distinguishes "already-sent content that survived the trim" from "genuinely new
// content" without looking at the actual bytes. So this is where the existing
// command_hash() dedup pattern (already used for git commits via git_seen_hashes.json)
// gets adapted for shell history: a small persisted set of hashes for lines we've already
// turned into events, checked before emitting anything, so re-reading the same surviving
// tail after a trim is a no-op instead of a flood of duplicate events.

// Capped rather than "remember every command hash forever", for two reasons. First, disk
// use: an unbounded set would grow for as long as the daemon runs, forever. Second, and
// more importantly, correctness: command_hash() only fingerprints the command TEXT
// (normalised whitespace), it doesn't include a timestamp, because plain bash/PowerShell
// history has no per-entry timestamp to include (see parse_plain_history_str(), every line
// gets stamped with Utc::now() at parse time, which differs on every re-parse and so can't
// be part of a stable fingerprint). That means two genuinely separate runs of the exact
// same command hash identically. An unbounded "seen forever" set would silently stop
// remembering a command entirely the first time it repeats, which would badly degrade
// recall() for the common case of re-running the same build/test command many times a day.
// Capping the set and evicting the oldest hash once it's full means a repeated command is
// only suppressed while it's still within the last SEEN_HASHES_CAP commands, comfortably
// enough to absorb a truncation-triggered re-read of the surviving tail, small enough that
// a command re-run hours or days later is treated as new again.
const SEEN_HASHES_CAP: usize = 5000;

/// Per-source metadata (file size and mtime as of the last successful read), persisted
/// alongside the existing byte-offset `.cursor` file. This exists purely for explicit,
/// loggable truncation detection, cursor::read_new_bytes() already self-heals a stale
/// offset on its own by resetting to 0, this just lets us notice and log when that
/// happened instead of it being a silent behavior change.
#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
struct SourceMeta {
    file_size: u64,
    #[serde(default)]
    mtime_secs: i64,
}

async fn read_source_meta(path: &Path) -> SourceMeta {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => SourceMeta::default(),
    }
}

async fn write_source_meta(path: &Path, meta: &SourceMeta) -> Result<()> {
    tokio::fs::write(path, serde_json::to_string(meta)?).await?;
    Ok(())
}

/// Stat's the history file's current size and mtime, and returns true if it's shrunk since
/// `previous` was recorded, the signal that the file was trimmed, rotated, or replaced
/// since we last read it.
async fn file_shrank_since(path: &Path, previous: &SourceMeta) -> (bool, SourceMeta) {
    let current = match tokio::fs::metadata(path).await {
        Ok(m) => SourceMeta {
            file_size: m.len(),
            mtime_secs: m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        },
        Err(_) => SourceMeta::default(),
    };
    (current.file_size < previous.file_size, current)
}

/// Loads the bounded, ordered set of already-ingested command hashes for one history
/// source. A missing or corrupt file just means "nothing remembered yet", same
/// fail-safe-open philosophy as every other piece of persisted local state in this daemon.
async fn read_seen_hashes(path: &Path) -> VecDeque<String> {
    match tokio::fs::read_to_string(path).await {
        Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
        Err(_) => VecDeque::new(),
    }
}

async fn write_seen_hashes(path: &Path, seen: &VecDeque<String>) -> Result<()> {
    tokio::fs::write(path, serde_json::to_string(seen)?).await?;
    Ok(())
}

/// Records `hash` as seen, evicting the oldest entry first if the set is already at
/// SEEN_HASHES_CAP. Kept as its own tiny function rather than inlined at the one call site
/// so the eviction rule is documented and tested in exactly one place.
fn record_seen_hash(seen: &mut VecDeque<String>, hash: String) {
    seen.push_back(hash);
    while seen.len() > SEEN_HASHES_CAP {
        seen.pop_front();
    }
}

// The main entry point called once per poll tick from daemon/mod.rs. Walks whatever shell
// history files exist for the current platform, reads only the NEW bytes since last time
// (via the byte-offset cursor, see POLISH-005 below), parses those bytes into structured
// entries, filters out anything matching an exclusion pattern (so secrets typed as env vars
// don't end up remembered forever), and turns what's left into events.
/// Reads new shell-history lines since the last poll, filters out excluded commands, and
/// converts what's left into events.
///
/// # Errors
///
/// Returns an error if the shell history file or cursor can't be read.
pub async fn poll_shell_history(config: &Config) -> Result<Vec<Event>> {
    poll_shell_history_with_home(config, &home_dir()).await
}

// The actual implementation, taking `home` as an explicit parameter instead of calling
// home_dir() internally. This is what lets test_unflushed_history_produces_zero_events_
// until_flushed (further down) point the poller at a tempdir directly, rather than having
// to mutate the real process-wide HOME env var and hope no other test reads it at the same
// time under cargo test's default parallel execution.
async fn poll_shell_history_with_home(config: &Config, home: &Path) -> Result<Vec<Event>> {
    let patterns = exclusion_patterns(config);

    let mut events = Vec::new();
    let data_dir = Config::data_dir()?;
    tokio::fs::create_dir_all(&data_dir).await?;

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

        let source_name = history_path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let cursor_path = data_dir.join(format!("{}.cursor", source_name));
        let last_dir_path = data_dir.join(format!("{}.lastdir", source_name));
        let meta_path = data_dir.join(format!("{}.meta.json", source_name));
        let seen_path = data_dir.join(format!("{}.seen.json", source_name));

        // SHELL-006: check for truncation explicitly before reading, purely so it's
        // logged and visible rather than a silent internal reset. read_new_bytes() below
        // will notice and self-heal the stale offset either way.
        let prev_meta = read_source_meta(&meta_path).await;
        let (shrank, current_meta) = file_shrank_since(history_path, &prev_meta).await;
        if shrank {
            warn!(
                "{} shrank since last read ({} -> {} bytes, trimmed/rotated/edited). \
                 Re-reading from the start of the file; already-ingested commands still \
                 in the surviving content will be skipped via the seen-hash set rather \
                 than re-sent to Cognee.",
                source_name, prev_meta.file_size, current_meta.file_size
            );
        }

        let byte_offset = cursor::read_cursor(&cursor_path).await;
        let (content, new_offset) = cursor::read_new_bytes(history_path, byte_offset).await?;
        if content.is_empty() {
            cursor::write_cursor(&cursor_path, new_offset).await?;
            write_source_meta(&meta_path, &current_meta).await?;
            continue;
        }

        // Parse the new content into entries
        let mut entries = match format {
            HistoryFormat::Zsh => parse_zsh_history_str(&content),
            HistoryFormat::Plain => parse_plain_history_str(&content),
        };

        // SHELL-005: reconstruct working directories from cd commands, picking up from
        // wherever the last poll tick left off rather than resetting to the daemon's own
        // static launch directory every time. See read_last_dir's doc comment for why that
        // matters. This intentionally runs over the FULL entry list, including anything
        // about to be filtered out as a duplicate below, a cd command that happens to be a
        // truncation-replay duplicate still needs to be replayed for directory tracking to
        // stay accurate, only whether we EMIT AN EVENT for a line is affected by dedup.
        let start_dir = read_last_dir(&last_dir_path).await;
        let end_dir = reconstruct_directories(&mut entries, start_dir, home);
        write_last_dir(&last_dir_path, &end_dir).await?;

        let mut seen_hashes = read_seen_hashes(&seen_path).await;

        for entry in &entries {
            if entry.command.is_empty() {
                continue;
            }
            if is_excluded(&entry.command, patterns) {
                continue;
            }

            let hash = command_hash(&entry.command);
            if seen_hashes.contains(&hash) {
                debug!("Skipping already-ingested command: {}", &entry.command);
                continue;
            }
            record_seen_hash(&mut seen_hashes, hash.clone());

            events.push(Event::ShellCommand(ShellCommandEvent {
                timestamp: entry.timestamp,
                directory: entry.directory.clone(),
                command: entry.command.clone(),
                exit_code: entry.exit_code,
                output: entry.stderr.clone(),
                duration_ms: entry.duration_ms,
                session_id: None,
                command_hash: Some(hash),
                error_type: entry.stderr.as_deref().and_then(classify_error),
            }));
            debug!("Shell event: {}", &entry.command);
        }

        write_seen_hashes(&seen_path, &seen_hashes).await?;
        cursor::write_cursor(&cursor_path, new_offset).await?;
        write_source_meta(&meta_path, &current_meta).await?;
    }

    Ok(events)
}

// Reads the directory reconstruct_directories() left off at on the previous poll tick, or
// falls back to the daemon's own current directory if there's no persisted value yet (the
// very first poll since the daemon started).
//
// Before this existed, reconstruct_directories re-derived std::env::current_dir() (the
// daemon PROCESS's own working directory, fixed at daemon launch and never changing again)
// as its starting point on every single call. Since each poll tick only replays the cd
// commands found in that tick's new content, an event would only get tagged with the right
// directory if a cd happened to be the very first new line since last time, otherwise every
// event in that batch inherited the daemon's stale launch directory instead of wherever the
// user actually was. Persisting the last reconstructed directory here means each tick
// continues from the truth the previous tick already worked out, instead of throwing that
// context away every 30-60 seconds.
async fn read_last_dir(path: &Path) -> PathBuf {
    match tokio::fs::read_to_string(path).await {
        Ok(s) if !s.trim().is_empty() => PathBuf::from(s.trim()),
        // No persisted state yet (first run for this history source). current_dir() is a
        // blocking std call, so it goes through spawn_blocking like every other blocking
        // call in this codebase, even though in practice it's fast enough that it'd be hard
        // to notice either way.
        _ => tokio::task::spawn_blocking(|| std::env::current_dir().unwrap_or_else(|_| home_dir()))
            .await
            .unwrap_or_else(|_| home_dir()),
    }
}

async fn write_last_dir(path: &Path, dir: &Path) -> Result<()> {
    tokio::fs::write(path, dir.to_string_lossy().as_bytes()).await?;
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
                    // reconstruct_directories() unconditionally overwrites this for every
                    // entry right after parsing, so there's no point spending a
                    // std::env::current_dir() syscall on a value that never survives to be
                    // read. An empty placeholder here costs nothing.
                    directory: String::new(),
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
                directory: String::new(), // overwritten by reconstruct_directories(), see above
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
            directory: String::new(), // overwritten by reconstruct_directories(), see above
            stderr: None,
        })
        .collect()
}

// ── SHELL-005: working directory tracking via cd command sequence ─────────────
// History files don't record which directory each command ran in, only the command text
// itself. So to give every event a meaningful `directory` field, we replay the sequence of
// commands starting from wherever we currently are, and whenever we spot a `cd` command we
// update our tracked "current" directory accordingly. It's an approximation (if the daemon
// wasn't running continuously since the very first command in history, our starting point
// is a guess) but it's good enough to be genuinely useful for recall().

// reconstruct_directories takes `home` as an explicit parameter (dependency injection)
// rather than calling home_dir() internally. Besides being the more testable shape in
// general, it's specifically what lets the tests below exercise `~/`-relative cd tracking
// against a controlled tempdir instead of having to mutate the real process HOME env var,
// which is process-global state that cargo test's default parallel test execution can't
// safely share across concurrently-running tests.
fn reconstruct_directories(
    entries: &mut Vec<HistoryEntry>,
    start_dir: PathBuf,
    home: &Path,
) -> PathBuf {
    let mut current = start_dir;

    for entry in entries.iter_mut() {
        entry.directory = current.to_string_lossy().to_string();

        if let Some(new_dir) = extract_cd_target(&entry.command, &current, home) {
            current = new_dir;
        }
    }

    current
}

// Handles the handful of cd forms people actually type: bare `cd` (goes home), `cd ~`,
// absolute paths (both Unix `/foo` and Windows `C:\foo`), home-relative `~/foo`, `cd ..`,
// and plain relative paths. Deliberately does NOT try to handle `cd -` (back to previous
// dir) since tracking that would need its own directory history stack, felt like overkill
// for what this feature needs to deliver.
fn extract_cd_target(cmd: &str, current: &Path, home: &Path) -> Option<PathBuf> {
    let cmd = cmd.trim();
    // Match bare `cd` or `cd <path>`; skip compound commands
    if cmd != "cd" && !cmd.starts_with("cd ") {
        return None;
    }

    let target = if cmd == "cd" { "" } else { cmd[3..].trim() };

    if target.is_empty() || target == "~" {
        return Some(home.to_path_buf());
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

    // Byte-cursor round-trip coverage now lives in daemon::cursor's own tests, since
    // read_cursor/write_cursor moved there as the one shared implementation every poller
    // uses. No need to duplicate that coverage here too.

    #[test]
    fn test_cd_tracking_home() {
        let home = PathBuf::from("/home/testuser");
        let current = PathBuf::from("/some/path");
        let result = extract_cd_target("cd", &current, &home);
        assert_eq!(result, Some(home));
    }

    #[test]
    fn test_cd_tracking_relative() {
        let home = PathBuf::from("/home/testuser");
        let current = PathBuf::from("/home/user");
        let result = extract_cd_target("cd projects", &current, &home);
        assert_eq!(result, Some(PathBuf::from("/home/user/projects")));
    }

    #[test]
    fn test_cd_tracking_absolute() {
        let home = PathBuf::from("/home/testuser");
        let current = PathBuf::from("/anywhere");
        let result = extract_cd_target("cd /tmp/work", &current, &home);
        assert_eq!(result, Some(PathBuf::from("/tmp/work")));
    }

    #[test]
    fn test_non_cd_returns_none() {
        let home = PathBuf::from("/home/testuser");
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
        let start = PathBuf::from("/home/user/project");
        let home = PathBuf::from("/home/testuser");
        let end = reconstruct_directories(&mut entries, start, &home);
        assert_eq!(entries[2].directory, "/tmp");
        assert_eq!(end, PathBuf::from("/tmp"));
    }

    // This is the actual bug the start_dir/end_dir plumbing exists to fix: a batch with no
    // cd command in it at all should still get tagged with wherever the PREVIOUS batch left
    // off, not silently reset to some unrelated default. Before this fix, reconstruct_directories
    // always reseeded from std::env::current_dir() on every call, so a batch like this one
    // would have been tagged with the daemon's own static launch directory instead of
    // "/home/user/deep/project" (wherever the user genuinely was).
    #[test]
    fn test_reconstruct_directories_continues_from_previous_tick() {
        let mut entries = vec![HistoryEntry {
            timestamp: Utc::now(),
            command: "cargo build".into(),
            exit_code: None,
            duration_ms: None,
            directory: String::new(),
            stderr: None,
        }];
        let carried_over = PathBuf::from("/home/user/deep/project");
        let home = PathBuf::from("/home/testuser");
        let end = reconstruct_directories(&mut entries, carried_over.clone(), &home);
        assert_eq!(entries[0].directory, "/home/user/deep/project");
        assert_eq!(end, carried_over);
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
        let _guard = BASH_HISTORY_STATE_LOCK.lock().unwrap();
        reset_bash_history_state();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let history_path = dir.path().join(".bash_history");
        std::fs::write(&history_path, "").unwrap(); // freshly created, nothing written yet

        let config = Config::default();
        let rt = tokio::runtime::Runtime::new().unwrap();

        // First, let's check the state everyone hits by accident: a command has been
        // typed but never flushed, because there's no history -a and the shell hasn't
        // exited. This is what a shell stuck on the old rc file looks like forever.
        let events = rt
            .block_on(poll_shell_history_with_home(&config, &home))
            .unwrap();
        assert_eq!(events.len(), 0, "nothing on disk yet, so nothing to poll");

        // Now let's simulate the fix actually working: PROMPT_COMMAND's history -a (or
        // zsh's INC_APPEND_HISTORY) fires, and the command finally lands on disk.
        std::fs::write(&history_path, "cargo build --release\n").unwrap();
        let events = rt
            .block_on(poll_shell_history_with_home(&config, &home))
            .unwrap();
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
        let events = rt
            .block_on(poll_shell_history_with_home(&config, &home))
            .unwrap();
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

    // TEST-002: poll_shell_history_with_home always resolves state files (cursor, lastdir,
    // and now SHELL-006's meta/seen files) under the REAL Config::data_dir(), not anything
    // scoped to the tempdir `home` a test passes in, only the history file path itself is
    // test-scoped. That means any two tests both exercising a ".bash_history" source share
    // the exact same on-disk state files, and cargo test runs tests in parallel by default,
    // so without serializing them, one test's writes can interleave with another's reads
    // and cause spurious failures that have nothing to do with either test's actual logic.
    // A process-wide mutex around just the two tests that hit this is the minimal fix,
    // properly parameterizing data_dir() for tests would be the more thorough fix but is a
    // bigger, riskier change than this bug warrants.
    static BASH_HISTORY_STATE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Deletes any state files left over in the real data dir from a previous test run (or
    /// a previous `cargo test` invocation entirely) for the ".bash_history" source, so each
    /// test using it starts from a clean, known slate rather than depending on whatever
    /// happened to be there before.
    fn reset_bash_history_state() {
        if let Ok(data_dir) = Config::data_dir() {
            for suffix in [".cursor", ".lastdir", ".meta.json", ".seen.json"] {
                let _ = std::fs::remove_file(data_dir.join(format!(".bash_history{}", suffix)));
            }
        }
    }

    #[test]
    fn test_command_hash_normalises() {
        use crate::events::command_hash;
        assert_eq!(command_hash("cargo  build"), command_hash("cargo build"));
    }

    // ── SHELL-006: truncation-safe dedup ────────────────────────────────────

    #[test]
    fn test_record_seen_hash_evicts_oldest_once_full() {
        let mut seen = VecDeque::new();
        for i in 0..SEEN_HASHES_CAP {
            record_seen_hash(&mut seen, format!("hash-{}", i));
        }
        assert_eq!(seen.len(), SEEN_HASHES_CAP);
        assert_eq!(seen.front().unwrap(), "hash-0");

        record_seen_hash(&mut seen, "hash-new".to_string());
        assert_eq!(
            seen.len(),
            SEEN_HASHES_CAP,
            "set should stay capped, not grow unbounded"
        );
        assert_eq!(
            seen.front().unwrap(),
            "hash-1",
            "oldest entry should have been evicted to make room"
        );
        assert!(seen.contains(&"hash-new".to_string()));
    }

    #[tokio::test]
    async fn source_meta_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("bash_history.meta.json");
        let meta = SourceMeta { file_size: 500, mtime_secs: 12345 };
        write_source_meta(&p, &meta).await.unwrap();
        let loaded = read_source_meta(&p).await;
        assert_eq!(loaded.file_size, 500);
        assert_eq!(loaded.mtime_secs, 12345);
    }

    #[tokio::test]
    async fn missing_source_meta_defaults_to_zero() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("does_not_exist.json");
        let loaded = read_source_meta(&p).await;
        assert_eq!(loaded.file_size, 0);
    }

    #[tokio::test]
    async fn file_shrank_since_detects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".bash_history");
        std::fs::write(&p, "a\nb\nc\n").unwrap();

        let previous = SourceMeta { file_size: 100, mtime_secs: 0 };
        let (shrank, current) = file_shrank_since(&p, &previous).await;
        assert!(shrank, "current 6-byte file is smaller than the recorded 100 bytes");
        assert_eq!(current.file_size, 6);
    }

    #[tokio::test]
    async fn file_shrank_since_false_when_grown_or_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".bash_history");
        std::fs::write(&p, "a\nb\nc\n").unwrap();

        let previous = SourceMeta { file_size: 3, mtime_secs: 0 };
        let (shrank, _) = file_shrank_since(&p, &previous).await;
        assert!(!shrank, "file grew, that's not a truncation");
    }

    // This is the actual bug report reproduced end to end: the daemon ingests some
    // commands, then (simulating a restart landing right after HISTFILESIZE trimmed the
    // history file) the file is replaced with a SHORTER file whose content is a subset of
    // what was already ingested. Before SHELL-006, read_new_bytes()'s own shrink-reset
    // meant this replayed as brand new content and every line got re-emitted as a
    // duplicate event. With the seen-hash set in place, the exact same lines should now
    // produce zero new events, only genuinely new content after the trim should surface.
    #[test]
    fn test_truncation_does_not_duplicate_already_ingested_commands() {
        let _guard = BASH_HISTORY_STATE_LOCK.lock().unwrap();
        reset_bash_history_state();
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().to_path_buf();
        let history_path = dir.path().join(".bash_history");

        std::fs::write(
            &history_path,
            "cargo build --release\ngit status\ncargo test\n",
        )
        .unwrap();

        let config = Config::default();
        let rt = tokio::runtime::Runtime::new().unwrap();

        let first_pass = rt
            .block_on(poll_shell_history_with_home(&config, &home))
            .unwrap();
        assert_eq!(first_pass.len(), 3, "all three commands ingested the first time");

        // Simulate HISTFILESIZE trimming the file down to just its last line right around
        // a daemon restart, the exact scenario from the bug report: the surviving content
        // ("cargo test") was already ingested above, but the file is now SHORTER than the
        // persisted cursor offset, which is what forces read_new_bytes() to reset to 0.
        std::fs::write(&history_path, "cargo test\n").unwrap();

        let second_pass = rt
            .block_on(poll_shell_history_with_home(&config, &home))
            .unwrap();
        assert_eq!(
            second_pass.len(),
            0,
            "the only surviving line was already ingested, so nothing new should be emitted"
        );

        // And a genuinely new command appended after the trim should still surface
        // normally, dedup must not swallow real new content.
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&history_path)
            .unwrap();
        use std::io::Write;
        writeln!(f, "cargo clippy").unwrap();

        let third_pass = rt
            .block_on(poll_shell_history_with_home(&config, &home))
            .unwrap();
        assert_eq!(third_pass.len(), 1, "genuinely new command after the trim should surface");
        if let Event::ShellCommand(sc) = &third_pass[0] {
            assert_eq!(sc.command, "cargo clippy");
        } else {
            panic!("expected a ShellCommand event");
        }
    }
}


--- ./src/daemon/buffer.rs ---
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


--- ./src/daemon/discovery.rs ---
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
/// Checks the most recent shell command against known package managers, kicking off
/// discovery in the background for anything unrecognized.
///
/// # Errors
///
/// Returns an error if reading the shell history cursor fails.
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


--- ./src/daemon/mod.rs ---
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

/// Runs the daemon's main event loop: polling shell history, package managers, and git,
/// batching results, and flushing them to Cognee on a timer until a shutdown signal arrives.
///
/// # Errors
///
/// Returns an error if the daemon fails to initialize (for example, an unreadable config).
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
/// Checks for and clears a force-flush signal file, resetting backoff so the next tick
/// flushes immediately regardless of the current retry gate.
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


--- ./src/daemon/cursor.rs ---
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


--- ./src/daemon/packages.rs ---
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

/// Records the most recently observed shell command, so a later package-manager poll can
/// correlate an install event with the command that triggered it.
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
/// Polls every known package manager for new installs/upgrades since the last check,
/// continuing past any single manager's failure.
///
/// # Errors
///
/// Returns an error only if every package manager poll fails.
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


--- ./src/events/mod.rs ---
//! The shared event schema every part of `bruh` serializes through.
//!
//! Small mod.rs here, nothing fancy. schema.rs holds the actual Event struct and its variants,
//! this file just declares it as a submodule and re-exports everything from it with the
//! wildcard so the rest of the codebase can do `use crate::events::Event` instead of having
//! to reach all the way into `crate::events::schema::Event`. Saves a bit of typing everywhere else.
pub mod schema;
pub use schema::*;


--- ./src/events/schema.rs ---
/// The shared vocabulary of the whole project.
///
/// Every single thing the daemon observes (a shell command, a package install, a git
/// commit, a discovered package manager) gets normalized into one of these variants before
/// it goes anywhere near Cognee. Having one enum for all of this means ingest.rs, buffer.rs,
/// and every poller can all just work with `Event` without caring about the specific shape
/// underneath until they actually need to.
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
/// A single observed shell command, with dedup hashing and error classification.
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
/// A package install/upgrade observed on a known or learned package manager.
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
/// Whether a package manager is one bruh knows natively, or one discovery learned on
/// the fly.
pub enum ManagerType {
    Bootstrapped,
    Learned,
}

/// SCHEMA-NEW-001 + GIT-003: working_directory + diff_summary.
#[derive(Debug, Clone, Serialize, Deserialize)]
/// A single observed git commit.
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
/// A package manager's profile as figured out by the discovery pipeline: how to install,
/// remove, and list packages with it.
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
/// How confident the LLM extractor was in a discovered [`PackageManagerProfile`].
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


--- ./src/cli/output.rs ---
//! CLI-004: Timeline output with ANSI colors and local time conversion.
//! CLI-NEW-002: UTC timestamps displayed as local time.
//! POLISH-007: NO_COLOR / TERM=dumb support.

use chrono::{DateTime, Local, Utc};

// ANSI escape codes, inline so we need zero extra dependencies.
//
// PALETTE-001: bruh's whole terminal look runs on three colors on purpose, not the usual
// red/yellow/green traffic light set. Green and cyan are close cousins on the color wheel,
// so they read as "the same family" even when they're marking different things (green for
// success, cyan for structure), and deep orange is the one color that's warm enough to grab
// attention for warnings and errors without being the alarm-red every other CLI already
// uses. Plain terminal white barely shows up at all, most body text just gets no color code
// added and rides on whatever the user's terminal default foreground is, which keeps things
// legible on both light and dark terminal themes instead of us guessing wrong.
//
// Deep orange isn't one of the 16 standard ANSI colors, there's no escape code for "deep
// orange" in the basic set, so this reaches into the 256-color palette instead (code 166,
// a burnt/rust orange that's dark enough to stay readable on both light and dark
// backgrounds, unlike the very bright default "orange-ish" 208 which can wash out on light
// terminals). Every terminal from the last ~15 years supports 256-color mode, so this is a
// safe bet.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const DEEP_ORANGE: &str = "\x1b[38;5;166m";

/// Whether ANSI color codes should be emitted, honoring `NO_COLOR`, `TERM=dumb`, and
/// whether stdout is actually a terminal.
pub fn is_color_enabled() -> bool {
    use std::io::IsTerminal;

    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    if matches!(std::env::var("TERM"), Ok(t) if t == "dumb") {
        return false;
    }
    // Piping output to a file or another program (`bruh query "..." > out.txt`, or
    // `| less`) shouldn't embed raw ANSI escape codes just because the user didn't also
    // remember to set NO_COLOR. std::io::IsTerminal has been stable since Rust 1.70, so
    // this needs no new dependency, just a check we weren't doing before.
    std::io::stdout().is_terminal()
}

fn c(code: &str, text: &str) -> String {
    if is_color_enabled() {
        format!("{}{}{}", code, text, RESET)
    } else {
        text.to_string()
    }
}

/// Wraps `s` in the bold ANSI escape, or returns it unchanged if color is disabled.
pub fn bold(s: &str) -> String {
    c(BOLD, s)
}
/// Wraps `s` in the dim ANSI escape, or returns it unchanged if color is disabled.
pub fn dim(s: &str) -> String {
    c(DIM, s)
}
/// Wraps `s` in green, or returns it unchanged if color is disabled.
pub fn green(s: &str) -> String {
    c(GREEN, s)
}
/// Wraps `s` in cyan, or returns it unchanged if color is disabled.
pub fn cyan(s: &str) -> String {
    c(CYAN, s)
}
// PALETTE-001: this is the one color for "pay attention", covers what red and yellow used
// to split between them (errors, warnings, disabled states, destructive confirmations, any
// non-zero exit code). One color for "something's off" is easier to keep consistent across
// a whole CLI than juggling where the red/yellow line falls in each file.
/// Wraps `s` in the deep-orange "pay attention" color, or returns it unchanged if color is
/// disabled.
pub fn orange(s: &str) -> String {
    c(DEEP_ORANGE, s)
}

/// Convert a UTC timestamp to local time formatted as HH:MM:SS.
pub fn fmt_time(ts: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = ts.with_timezone(&Local);
    local.format("%H:%M:%S").to_string()
}

/// Convert a UTC timestamp to a human-readable local date/time.
pub fn fmt_datetime(ts: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = ts.with_timezone(&Local);
    local.format("%a %b %d · %H:%M").to_string()
}

/// Prints a dim horizontal rule used to frame CLI section headers and footers.
pub fn print_divider() {
    println!("{}", dim(&"─".repeat(56)));
}

/// Prints the standard `bruh · <title>` banner, framed by dividers above and below.
pub fn print_header(title: &str) {
    print_divider();
    println!("  {}  ·  {}", bold(&cyan("bruh")), bold(title));
    print_divider();
}

/// Prints the closing divider that pairs with [`print_header`].
pub fn print_footer() {
    print_divider();
}

/// Print an exit code badge: green \[0\] or deep orange \[N\].
pub fn exit_badge(code: i32) -> String {
    if code == 0 {
        green(&format!("[{}]", code))
    } else {
        orange(&format!("[{}]", code))
    }
}

/// Render a full Cognee recall() response as a human-readable timeline.
/// The response may be plain text, a JSON object with a "text" key, or arbitrary JSON.
pub fn print_timeline(response: &serde_json::Value, raw: bool) {
    if raw {
        if let Ok(s) = serde_json::to_string_pretty(response) {
            println!("{}", s);
        }
        return;
    }

    print_header("Memory Query");
    println!();

    // Try to extract meaningful text from the response.
    let text = extract_text(response);

    if text.trim().is_empty() {
        println!("  {}  No memory found for that query.", orange("○"));
        println!();
        println!(
            "  {}  Make sure the daemon is running: {}",
            dim("tip"),
            bold("bruh daemon &")
        );
    } else {
        // Render line by line, annotating patterns we recognise.
        for line in text.lines() {
            render_line(line);
        }
    }

    println!();
    print_footer();
}

fn render_line(line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        println!();
        return;
    }

    // Session header pattern: "Session: ..."
    if trimmed.starts_with("Session:") || trimmed.starts_with("session_") {
        println!("  {}", bold(&cyan(trimmed)));
        return;
    }

    // Error lines
    if trimmed.starts_with("error") || trimmed.starts_with("Error") || trimmed.starts_with("ERROR")
    {
        println!("  {}  {}", orange("✗"), orange(trimmed));
        return;
    }

    // Git commit lines. Cyan here rather than a warning color, a commit isn't bad news,
    // it's just a structural event worth calling out visually from the surrounding prose.
    if trimmed.starts_with("git commit") || trimmed.contains("commit -m") {
        println!("  {}  {}", cyan("◆"), bold(&cyan(trimmed)));
        return;
    }

    // Exit code patterns [0] / [1]
    if trimmed.contains("[0]") {
        println!(
            "  {}  {}",
            green("✓"),
            trimmed.replace("[0]", &green("[0]"))
        );
        return;
    }
    if trimmed.contains("[1]") || trimmed.contains("[2]") {
        let annotated = trimmed
            .replace("[1]", &orange("[1]"))
            .replace("[2]", &orange("[2]"));
        println!("  {}  {}", orange("✗"), annotated);
        return;
    }

    // Timestamp-prefixed lines: "14:32:18  command..."
    let parts: Vec<&str> = trimmed.splitn(2, "  ").collect();
    if parts.len() == 2 {
        let ts = parts[0].trim();
        // Simple heuristic: timestamp looks like HH:MM:SS
        if ts.len() == 8 && ts.chars().filter(|&c| c == ':').count() == 2 {
            println!("  {}  {}", dim(ts), bold(parts[1].trim()));
            return;
        }
    }

    // Default: Markdown-aware indent instead of dumping raw ** and - characters.
    println!("  {}", markdown_to_terminal(trimmed));
}

fn extract_text(v: &serde_json::Value) -> String {
    // Try common response fields in order of preference.
    for key in &["text", "result", "answer", "response", "content", "message"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    // Try nested results array
    if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .map(|s| s.to_string())
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    // Fall back to pretty JSON
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Print a neat stats summary box.
pub fn print_stats_box(lines: &[(&str, String)]) {
    print_header("Developer Activity Report");
    println!();
    for (label, value) in lines {
        // Apply markdown rendering to the value
        let rendered_value = markdown_to_terminal(&value);
        println!(
            "  {}  {}",
            cyan(&format!("{:<28}", label)),
            bold(&rendered_value)
        );
    }
    println!();
    print_footer();
}

/// COGNEE-018: recall() responses are LLM-generated prose meant for a chat UI,
/// headings and emphasis come back as literal Markdown ("**Developer Hand-off
/// Brief**", "- Last-session activity"), which just showed up as raw asterisks and
/// dashes on screen. This renders the common cases (bold, bullets) into something
/// that actually reads cleanly on a terminal instead of dumping the Markdown as-is.
fn markdown_to_terminal(line: &str) -> String {
    let trimmed = line.trim_start();
    let (prefix, rest) = if let Some(stripped) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        (cyan("• "), stripped)
    } else {
        (String::new(), trimmed)
    };

    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars().peekable();
    let mut bold_open = false;
    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume the second '*'
            if is_color_enabled() {
                out.push_str(if bold_open { RESET } else { BOLD });
            }
            bold_open = !bold_open;
        } else {
            out.push(ch);
        }
    }
    if bold_open && is_color_enabled() {
        out.push_str(RESET); // unterminated **, don't bleed bold into later output
    }
    format!("{}{}", prefix, out)
}

/// Print the bruh explain brief.
pub fn print_explain(directory: &str, narrative: &str) {
    print_header(&format!("Context Brief  ·  {}", directory));
    println!();
    for line in narrative.lines() {
        if line.trim().is_empty() {
            println!();
        } else {
            println!("  {}", markdown_to_terminal(line));
        }
    }
    println!();
    print_footer();
}

/// Inline bruh watch annotation.
pub fn print_watch_memory(header: &str, body: &str) {
    let bar = if is_color_enabled() {
        format!("{}{}{}", CYAN, "── bruh memory ", RESET)
    } else {
        "── bruh memory ".to_string()
    };
    let fill = "─".repeat(56usize.saturating_sub(15));
    println!("{}{}", bar, dim(&fill));
    println!("  {}", bold(header));
    for line in body.lines() {
        println!("  {}", line);
    }
    let close = if is_color_enabled() {
        format!("{}{}{}", CYAN, "─".repeat(56), RESET)
    } else {
        "─".repeat(56)
    };
    println!("{}", close);
}


--- ./src/cli/explain.rs ---
//! CLI-006: bruh explain, session handoff brief for the current directory.
// This is my favorite command in the whole project honestly. The idea: you sit down at
// your desk, cd into a project you haven't touched in a few days, run `bruh explain`, and
// it hands you back a plain-language brief of what you were doing last, without you having
// to scroll back through shell history or git log yourself trying to remember. It's really
// just a specially crafted recall() prompt scoped to the current directory, all the real
// intelligence is Cognee's, we're just asking the right question.

use crate::{
    cli::output::{dim, print_explain},
    cognee::recall,
};
use anyhow::Result;
use std::io::Write;

/// Runs `bruh explain`: a plain-language session handoff brief for the current directory.
pub async fn run() -> Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());

    // Same "show something while we wait" pattern as cli/query.rs's Thinking indicator,
    // GRAPH_COMPLETION queries aren't instant and a silent terminal makes it look stuck.
    print!("  {} Scanning memory for {}… ", dim("→"), dim(&cwd));
    std::io::stdout().flush()?;

    // Build a targeted recall query scoped to this directory
    // This prompt is doing a lot of work here, it's explicitly telling Cognee's graph
    // completion what shape of answer we want (last session's work, most recent commit,
    // packages, errors, unfinished business) and how to format it, rather than just
    // asking a vague "what happened here" and hoping for something coherent back.
    let query = format!(
        "Generate a developer context brief for the project in directory '{}'. \
         Include: what was being worked on last session, the most recent git commit, \
         any packages installed, errors encountered, and what was left unfinished. \
         Format as a concise handoff brief a developer would read before resuming work.",
        cwd
    );

    let response = recall(&query).await?;
    println!("done\n");

    let narrative = extract_narrative(&response);
    print_explain(&cwd, &narrative);

    Ok(())
}

// recall() already runs its own normalise_response (see cognee/query.rs), but I keep this
// second, slightly different extraction here as a belt-and-suspenders fallback since
// explain's use case is a bit more sensitive to getting SOME readable text back rather than
// silently failing, worst case we just pretty-print the raw JSON rather than showing
// nothing at all.
fn extract_narrative(v: &serde_json::Value) -> String {
    for key in &["text", "result", "answer", "response", "content"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "No context available.".into())
}


--- ./src/cli/stats.rs ---
//! CLI-005: real productivity summary from Cognee.
// `bruh stats` asks Cognee to summarize everything it knows about you into a structured
// productivity snapshot: how many commands, how many packages, commit count, session count,
// what tends to go wrong, how fast you fix it, when you're most active. We're leaning on
// the LLM behind recall() to do the actual aggregation rather than computing these numbers
// ourselves from raw events, since Cognee's graph already has the full picture and doing it
// there means we don't need to duplicate that logic locally.

use crate::{cli::output::print_stats_box, cognee::recall};
use anyhow::Result;

// The exact keys build_stats_table() below looks for. Kept as one list so the prompt and
// the parser can't quietly drift apart the way they could if these were just typed out
// twice.
const STATS_FIELDS: &[(&str, &str)] = &[
    ("Total commands", "commands"),
    ("Packages installed", "packages_installed"),
    ("Git commits", "git_commits"),
    ("Sessions", "sessions"),
    ("Most common error", "most_common_error"),
    ("Avg fix time", "avg_fix_time"),
    ("Most productive hour", "most_productive_hour"),
    ("Longest session", "longest_session"),
];

/// Runs `bruh stats`, asking Cognee to summarize ingested activity into a productivity snapshot.
pub async fn run() -> Result<()> {
    // Earlier this just described the fields we wanted in prose ("Include: total commands,
    // packages installed by manager, ...") and hoped the model would happen to answer back
    // with matching JSON. It sometimes did and sometimes didn't, since nothing in the prompt
    // actually told it to respond in JSON at all, so the clean table path below was mostly
    // going unused and falling back to raw text instead. Spelling out the exact keys and
    // saying "respond with only JSON" gives the model a format it can't really misread,
    // so the table renders far more often.
    let keys = STATS_FIELDS
        .iter()
        .map(|(_, key)| *key)
        .collect::<Vec<_>>()
        .join(", ");
    let prompt = format!(
        "Give me a structured summary of all developer activity. Respond with a single \
         JSON object and nothing else, no prose before or after it, using exactly these \
         keys: {}. Use your best estimate for any value you can't determine precisely, \
         omit a key entirely rather than guessing wildly if you have no signal for it at all.",
        keys
    );

    let response = recall(&prompt).await?;
    let text = extract_text(&response);
    render_stats_table(&text);
    Ok(())
}

/// Render a clean table from the stats response.
fn render_stats_table(text: &str) {
    let json = extract_json(text);

    if let Some(lines) = build_stats_table(&json) {
        print_stats_box(&lines);
    } else {
        // No structured data found, fall back to raw text inside the same box styling
        // the rest of the CLI uses, rather than a bare, uncolored dump.
        crate::cli::output::print_header("Developer Activity Report");
        println!();
        print_raw_stats(text);
        println!();
        crate::cli::output::print_footer();
    }
}

/// Pulls whichever of the known fields are actually present in the parsed JSON into the
/// (label, value) pairs print_stats_box() wants. Returns None if nothing matched at all, so
/// the caller knows to fall back to raw text instead of printing an empty box.
fn build_stats_table(json: &serde_json::Value) -> Option<Vec<(&'static str, String)>> {
    if json.is_null() {
        return None;
    }

    let mut lines = Vec::new();
    for (label, key) in STATS_FIELDS {
        if let Some(val) = json.get(key) {
            let s = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            lines.push((*label, s));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

/// Fallback: print raw text when JSON parsing fails.
fn print_raw_stats(text: &str) {
    for line in text.lines().take(30) {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            println!("  {}", trimmed);
        }
    }
}

/// Extract JSON from the LLM response, handling prose wrapping.
fn extract_json(text: &str) -> serde_json::Value {
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                return v;
            }
        }
    }
    serde_json::Value::Null
}

/// Extract the raw text from a Cognee response, checking common fields.
fn extract_text(v: &serde_json::Value) -> String {
    for key in &["text", "result", "answer", "response"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    serde_json::to_string_pretty(v).unwrap_or_default()
}


--- ./src/cli/managers.rs ---
//! CLI-NEW-003: rich bruh managers table (no flag).
//! DISCOVERY-005: verbose --learn output.
// Two jobs live in this file: `bruh managers` on its own just lists what package managers
// we know about (built-in plus learned), and `bruh managers --learn <name>` forces
// discovery for a specific name on demand with full step-by-step output, basically running
// the same pipeline daemon/discovery.rs triggers silently in the background, but here for a
// human to actually watch happen.

use crate::{
    cli::output::{bold, cyan, dim, green, orange, print_footer, print_header},
    discovery::BOOTSTRAPPED_MANAGERS,
};
use anyhow::Result;

/// Runs `bruh managers`, either listing known package managers or, with `learn`, forcing discovery for one name.
pub async fn run(learn: Option<String>) -> Result<()> {
    if let Some(name) = learn {
        return run_learn(&name).await;
    }
    run_list()
}

// The default, no-flag view: a quick table of what's known. Bootstrapped managers get a
// simple space-joined line since there's nothing dynamic about them, learned managers get
// a proper table with provider, confidence, and discovery date, since that's the stuff a
// user would actually want to compare between entries.
fn run_list() -> Result<()> {
    print_header("Known Package Managers");
    println!();

    println!("  {} (always available):", bold("Bootstrapped"));
    println!("  {}", dim(&BOOTSTRAPPED_MANAGERS.join("  ")));
    println!();

    // Load learned managers from cache
    let learned = crate::discovery::cache::load_learned_managers().unwrap_or_default();

    if learned.is_empty() {
        println!("  {} No learned managers yet.", dim("Learned"));
        println!();
        println!(
            "  {}",
            dim("Run a command with an unknown package manager, or:")
        );
        println!("  {}  {}", dim("→"), bold("bruh managers --learn <name>"));
    } else {
        println!("  {} (discovered at runtime):", bold("Learned"));
        println!();

        // Header row
        // Fixed-width columns via format!("{:<width$}", ...) rather than a real table
        // library, keeps the dependency count down and the terminal output stays aligned
        // regardless of how many learned managers there are.
        let col = [28, 10, 8, 20];
        let headers = ["Name", "Provider", "Confidence", "Discovered"];
        let header_line: String = headers
            .iter()
            .zip(col.iter())
            .map(|(h, w)| format!("{:<width$}", h, width = *w))
            .collect::<Vec<_>>()
            .join("  ");
        println!("  {}", bold(&cyan(&header_line)));
        println!("  {}", dim(&"─".repeat(header_line.len())));

        for (name, profile) in &learned {
            let provider = profile
                .discovered_by_provider
                .as_deref()
                .unwrap_or("unknown");
            let confidence = profile.confidence.to_string();
            // Converting to Local time here specifically because discovered_at is stored
            // as UTC (see events/schema.rs), and a human reading "discovered at" wants
            // their own local time, not UTC.
            let discovered = {
                use chrono::{DateTime, Local};
                let local: DateTime<Local> = profile.discovered_at.with_timezone(&Local);
                local.format("%Y-%m-%d %H:%M").to_string()
            };

            let row = format!(
                "{:<28}  {:<10}  {:<8}  {}",
                green(name),
                provider,
                confidence,
                dim(&discovered)
            );
            println!("  {}", row);
        }
    }

    println!();
    print_footer();
    Ok(())
}

/// DISCOVERY-005: verbose step-by-step output for --learn.
// This mirrors what daemon/discovery.rs does silently in the background, but every step is
// printed here: the search, a couple of sample snippets so the user can sanity-check what
// the LLM is working from, the cascade attempt (via extract_with_cascade_verbose, see
// extractor.rs), the resulting profile fields, and finally storing it both in Cognee and
// the local cache. Good for debugging discovery when it's giving weird results, since you
// can actually see which provider answered and what it based its answer on.
async fn run_learn(name: &str) -> Result<()> {
    println!();
    println!(
        "  {} Discovering package manager: {}",
        bold("→"),
        bold(name)
    );
    println!();

    // Step 1: build context for the LLM cascade
    // DISCOVERY-009: this used to be a DuckDuckGo search step first. I pulled that out
    // entirely (see the DISCOVERY-009 note in discovery/mod.rs for the full reasoning),
    // the short version is that DDG's Instant Answer API isn't built for CLI tool
    // trivia, so it timed out or came back empty for most package manager names anyway,
    // and every provider in the cascade below already knows npm, cargo, pip, and their
    // smaller cousins from training. Asking the LLM directly skips a whole flaky network
    // hop for something it can usually just answer.
    println!(
        "  {} Skipping web search, asking the LLM cascade directly instead.",
        dim("→")
    );
    use std::io::Write;
    std::io::stdout().flush()?;

    let snippets = crate::discovery::direct_knowledge_prompt_context(name);
    println!();

    // Step 2: LLM extraction
    println!("  Running LLM extraction cascade:");
    let profile =
        crate::discovery::extractor::extract_with_cascade_verbose(name, &snippets).await?;
    println!();

    // Step 3: Show extracted profile
    println!("  {} Extracted profile:", bold("Result"));
    println!("    install verb:   {}", bold(&profile.install_verb));
    println!("    remove verb:    {}", profile.remove_verb);
    println!("    list command:   {}", profile.list_command);
    if let Some(ref lp) = profile.log_path {
        println!("    log path:       {}", lp);
    }
    if let Some(ref rp) = profile.registry_path {
        println!("    registry:       {}", rp);
    }
    println!("    confidence:     {}", profile.confidence);
    println!();

    // Step 4: Store
    // Both stores are attempted independently and neither failure stops the other, worst
    // case the user ends up with a locally cached profile even if Cognee's temporarily
    // down, or vice versa, better than an all-or-nothing failure on this step.
    print!("  Storing in Cognee graph… ");
    std::io::stdout().flush()?;
    match crate::discovery::register::store_profile(&profile).await {
        Ok(crate::discovery::register::StoreOutcome::Stored) => println!("{}", green("✓")),
        // This used to print the same green checkmark as an actual store, which meant
        // running --learn without a Cognee key configured looked exactly like success.
        // Now it says plainly that the store was skipped, and how to fix it.
        Ok(crate::discovery::register::StoreOutcome::NotConfigured) => {
            println!(
                "{} (skipped, no Cognee key configured, run {})",
                orange("○"),
                bold("bruh init")
            );
        }
        Err(e) => println!("{} ({})", orange("✗"), e),
    }

    print!("  Caching locally… ");
    std::io::stdout().flush()?;
    match crate::discovery::cache::save_learned_manager(&profile) {
        Ok(()) => println!("{}", green("✓")),
        Err(e) => println!("{} ({})", orange("✗"), e),
    }

    println!();
    println!(
        "  {} Learned {}. Run {} to see all managers.",
        green("✓"),
        bold(name),
        bold("bruh managers")
    );
    println!();

    Ok(())
}


--- ./src/cli/init.rs ---
//! INIT-NEW-001: daemon autostart.  INIT-NEW-002: API key validation.
//! POLISH-004: Windows shell profile + conditional chmod.
//! GIT-005: --force flag reinstalls the hook.
// This is the onboarding wizard, the first thing a new user runs. It walks through getting
// a Cognee API key configured, checking which LLM providers are available for discovery,
// installing the git hook so commits get picked up in real time, and optionally wiring the
// daemon to autostart whenever a terminal opens. Everything here is meant to be forgiving:
// skip a step and bruh should still work, just with reduced functionality (no discovery
// without an LLM key, no real-time git without the hook, etc), rather than refusing to run
// at all.

use crate::cli::{
    output::{bold as b, dim as d, green as g, orange},
    Config,
};
use anyhow::{Context, Result};
use std::{
    env,
    io::{self, Write},
};

/// Runs `bruh init`, the onboarding wizard for API keys, discovery providers, the git hook, and daemon autostart.
pub fn run() -> Result<()> {
    run_with_force(false)
}

/// GIT-005: --force reinstalls the git hook even if already present.
pub fn run_force() -> Result<()> {
    run_with_force(true)
}

// The actual wizard. Broken into clearly marked sections (API key, LLM providers, git hook,
// autostart, save) that each print their own status as they go, so even if something later
// fails the user can see exactly how far it got and what succeeded already.
fn run_with_force(force: bool) -> Result<()> {
    println!("\n{}", b("  bruh init"));
    println!("{}\n", d("  Configuring persistent developer memory\n"));

    let mut config = Config::load()?;

    // ── Cognee API key ─────────────────────────────────────────────
    // We check the environment first since someone might already have COGNEE_API_KEY set
    // globally, then fall back to whatever's already saved in config (unless --force is
    // set, in which case we always re-prompt), and only ask the user to type one in as a
    // last resort.
    let existing_key = env::var("COGNEE_API_KEY")
        .or_else(|_| env::var("BRUH_COGNEE_API_KEY"))
        .unwrap_or_default();

    let api_key = if !existing_key.is_empty() {
        println!("  {} Found COGNEE_API_KEY in environment", g("✓"));
        existing_key
    } else if !config.cognee_api_key.is_empty() && !force {
        println!("  {} Cognee API key already configured", g("✓"));
        config.cognee_api_key.clone()
    } else {
        println!("  {} Cognee API key not set.", orange("○"));
        println!("    Get one at: {}", b("https://app.cognee.ai"));
        print!("  Enter your Cognee API key (blank to skip): ");
        io::stdout().flush()?;
        let mut key = String::new();
        io::stdin().read_line(&mut key)?;
        key.trim().to_string()
    };

    if !api_key.is_empty() {
        config.cognee_api_key = api_key.clone();
        // INIT-NEW-002: validate key works
        // We do a live check against Cognee before saving, so the user finds out
        // immediately if they fat-fingered the key rather than discovering it hours later
        // when the daemon's first flush silently fails.
        print!("  Validating API key… ");
        io::stdout().flush()?;
        match validate_cognee_key(&api_key, &config.cognee_api_url) {
            Ok(()) => println!("{}", g("✓")),
            Err(e) => {
                println!("{}", orange("✗"));
                println!("  {} Validation failed: {}", orange("!"), e);
                println!(
                    "    Fix later: {}",
                    b("bruh config set cognee_api_key <key>")
                );
            }
        }
    } else {
        // No key at all means Cognee calls will just fail, so we disable discovery
        // upfront rather than letting the daemon repeatedly hit errors it has no chance
        // of recovering from.
        println!("  {} Skipping key — discovery disabled", d("–"));
        config.discovery_enabled = false;
    }

    // ── LLM providers ──────────────────────────────────────────────
    // Just a status check here, not a prompt, we can't really "set" these interactively
    // since they're API keys from three different companies, so we just tell the user
    // which ones we found configured in their environment already and point them at where
    // to get one for free if none are set.
    println!();
    let providers = [
        ("GOOGLE_AI_API_KEY", "gemini"),
        ("GROQ_API_KEY", "groq"),
        ("ANTHROPIC_API_KEY", "claude"),
    ];
    let mut found = 0usize;
    for (var, name) in &providers {
        if env::var(var).is_ok() {
            println!("  {} {} ({})", g("✓"), b(name), d(var));
            found += 1;
        } else {
            println!("  {} {} — set {} to enable", d("–"), name, d(var));
        }
    }
    if found == 0 {
        println!(
            "\n  {} Discovery disabled — configure a provider first.",
            orange("○")
        );
        println!(
            "    Free: {} or {}",
            b("aistudio.google.com"),
            b("console.groq.com")
        );
        config.discovery_enabled = false;
    }

    // ── Git hook ───────────────────────────────────────────────────
    println!();
    match install_git_hook(force) {
        Ok(true) => println!("  {} git post-commit hook installed", g("✓")),
        Ok(false) => println!("  {} Not in a git repo (hook skipped)", d("–")),
        Err(e) => println!("  {} Hook install failed: {}", orange("○"), e),
    }

    // ── INIT-NEW-001 / SHELL-006: daemon-supporting shell content ──
    // This used to be gated behind a "y/N" prompt, which meant the daemon could only ever
    // see whatever's already in your shell history file, since neither bash nor zsh write
    // to that file after every command by default (both only flush it when the shell
    // exits, unless something turns on incremental writes). So on a long-running terminal
    // session, the daemon would sit there polling a history file that never changes until
    // you close the window. That's most of why cargo/package events showed up reliably
    // (daemon/packages.rs polls Cargo.lock and the registry directly, it doesn't depend on
    // shell history at all) while plain shell commands and everything downstream of them
    // just didn't. install_shell_integration() below writes both the daemon autostart line
    // and the shell-specific "flush history immediately" directive in one go, no prompt,
    // so this is no longer something a user has to know to opt into.
    println!();
    match install_shell_integration(force) {
        Ok(Some(p)) => {
            println!("  {} Daemon + shell integration added to {}", g("✓"), b(&p));
            // Here's the thing worth spelling out for whoever's reading this later. We
            // just wrote a new PROMPT_COMMAND (or INC_APPEND_HISTORY for zsh) into that
            // profile file, but writing it to disk doesn't make it real yet. Your current
            // shell already loaded its profile a while ago and isn't going to notice this
            // change on its own. So if we don't say anything, the natural next step is
            // "run bruh daemon &, type a few commands, wonder why nothing's showing up,"
            // which is basically the whole bug this comment exists to prevent.
            println!(
                "  {} New shell setting won't apply to this session. Run {} or open a new terminal.",
                orange("○"),
                b(&format!("source {}", p))
            );
        }
        Ok(None) => {
            println!(
                "  {} Daemon + shell integration already present, skipped",
                d("–")
            );
            // Same story as above, just for the "it's already there" path. Someone could
            // easily be sitting in the exact same shell session that was open the first
            // time bruh init ever ran, in which case the incremental-flush setting has
            // never actually been loaded, even though the line has been sitting in their
            // profile the whole time. Worth a nudge rather than letting them assume it's
            // working.
            println!(
                "  {} If commands still aren't showing up, make sure this shell has re-sourced its profile since bruh init first ran.",
                orange("○")
            );
        }
        Err(e) => println!("  {} Could not update shell profile: {}", orange("○"), e),
    }

    // ── Save ───────────────────────────────────────────────────────
    config.save()?;
    println!("\n  {} Config saved", g("✓"));
    println!("  {} Run {}\n", b("→"), b("bruh daemon &"));
    Ok(())
}

// Rather than requiring a "real" auth endpoint, we just hit Cognee's /health with the
// bearer token attached and treat anything under a server error (500+) as "the key is at
// least accepted enough to reach the server." Connection errors or timeouts get treated as
// "can't tell, assume it's fine" rather than "the key is bad", since a flaky network
// shouldn't make init look like it failed over a perfectly good key.
fn validate_cognee_key(key: &str, api_url: &str) -> Result<()> {
    // Let's talk through why this function needs the wrapper below, because it's a
    // sneaky one. bruh init runs inside an async main (that's what #[tokio::main] on
    // main.rs gives us), but this function is plain synchronous code that reaches for
    // reqwest::blocking to make one quick HTTP call. reqwest::blocking works by quietly
    // spinning up its own little tokio runtime behind the scenes and waiting on it. The
    // problem is, tokio doesn't allow you to build or tear down one runtime while
    // you're already running inside another one on the same thread. It's not just
    // unsupported, it will panic, and it panics every single time, not once in a while.
    // tokio::task::block_in_place is the sanctioned escape hatch for exactly this
    // situation. It tells the multi-threaded runtime "step aside, this next bit of code
    // is going to block, hand this task off to a different worker so nothing gets
    // wedged." That's all we need here since the outer runtime is already running with
    // the rt-multi-thread feature.
    tokio::task::block_in_place(|| {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .context("HTTP client build failed")?;
        let url = format!("{}/health", api_url.trim_end_matches('/'));
        match client
            .get(&url)
            .header("Authorization", format!("Bearer {}", key))
            .send()
        {
            Ok(r) if r.status().as_u16() < 500 => Ok(()),
            Ok(r) => anyhow::bail!("HTTP {}", r.status()),
            Err(e) if e.is_connect() || e.is_timeout() => Ok(()), // server unreachable ≠ bad key
            Err(e) => anyhow::bail!("{}", e),
        }
    })
}

// Copies the bundled hooks/post-commit script (embedded into the binary at compile time via
// include_str!, so there's no risk of the hook file going missing at runtime) into the
// repo's .git/hooks/post-commit. If we're not inside a git repo at all, git rev-parse fails
// and we just return Ok(false) rather than treating that as an error, since not being in a
// repo is a perfectly normal state, not a problem.
fn install_git_hook(force: bool) -> Result<bool> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output();
    let output = match out {
        Ok(o) if o.status.success() => o,
        _ => return Ok(false),
    };

    let git_dir = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let hooks_dir = format!("{}/hooks", git_dir);
    std::fs::create_dir_all(&hooks_dir)?;

    let hook_path = format!("{}/post-commit", hooks_dir);

    // GIT-005: skip if already installed (unless --force)
    // We check that the existing hook actually mentions "bruh" before treating it as
    // already installed, otherwise we'd risk silently clobbering someone's pre-existing
    // custom post-commit hook that has nothing to do with us.
    if !force && std::path::Path::new(&hook_path).exists() {
        let existing = std::fs::read_to_string(&hook_path).unwrap_or_default();
        if existing.contains("bruh") {
            return Ok(true); // already ours
        }
    }

    let hook_content = include_str!("../../hooks/post-commit");
    std::fs::write(&hook_path, hook_content)?;

    // chmod +x only makes sense on Unix, git hooks need to be executable there, but
    // Windows doesn't use the same permission bit model at all, so this whole block is
    // conditionally compiled out entirely on Windows rather than being a no-op at runtime.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&hook_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&hook_path, perms)?;
    }
    Ok(true)
}

/// INIT-NEW-001 / SHELL-006: install everything the daemon needs from the shell side.
/// Returns Ok(Some(path)) if something was written, Ok(None) if it was already there and
/// force wasn't set, so run_with_force() can tell the two apart in its status line.
// Everything bruh adds lives between a pair of marker comments, that's what makes --force
// safe: instead of trying to guess which lines are "ours" by matching fragments of text,
// we just look for the whole marked block and swap it out wholesale. It also means anyone
// reading their own .bashrc later can tell at a glance exactly what bruh touched and where
// it starts and ends, nothing sneaks in unmarked.
const BLOCK_START: &str = "# >>> bruh daemon (managed, do not edit between markers) >>>";
const BLOCK_END: &str = "# <<< bruh daemon (managed, do not edit between markers) <<<";

fn install_shell_integration(force: bool) -> Result<Option<String>> {
    #[cfg(windows)]
    {
        // PowerShell's history already gets written to ConsoleHost_history.txt after every
        // command by default (no bash/zsh-style "only on exit" gotcha to work around here),
        // so the Windows side of this only needs the daemon autostart line, nothing extra
        // to flush.
        let profile_dir = std::env::var("USERPROFILE")
            .map(|h| std::path::PathBuf::from(h).join("Documents/PowerShell"))
            .unwrap_or_else(|_| std::path::PathBuf::from("."));
        std::fs::create_dir_all(&profile_dir)?;
        let profile = profile_dir.join("Microsoft.PowerShell_profile.ps1");
        let block = format!(
            "{}\nStart-Process bruh -ArgumentList 'daemon' -WindowStyle Hidden\n{}\n",
            BLOCK_START, BLOCK_END
        );
        return write_managed_block(&profile, &block, force);
    }
    #[cfg(not(windows))]
    {
        // Defaulting to .bashrc unless SHELL clearly says zsh, this covers the two shells
        // the vast majority of people are actually running day to day.
        let shell = env::var("SHELL").unwrap_or_default();
        let home = crate::cli::config::home_dir();
        let (profile, history_fix) = if shell.contains("zsh") {
            (
                home.join(".zshrc"),
                // zsh's default is the same "only write history at shell exit" behavior as
                // bash. INC_APPEND_HISTORY turns that off, each command lands in
                // .zsh_history right after it runs, which is what the daemon actually
                // needs since it polls that file on a timer, not on shell exit.
                "# Write each command to .zsh_history immediately, bruh's daemon polls\n\
                 # this file on a timer and can only see what's actually been written.\n\
                 setopt INC_APPEND_HISTORY"
                    .to_string(),
            )
        } else {
            (
                home.join(".bashrc"),
                // Plain bash only appends to .bash_history when the shell exits, unless
                // something calls `history -a` more often. Chaining it onto PROMPT_COMMAND
                // (which bash runs before every prompt redraw, so effectively after every
                // command) means it fires constantly instead of once per session. The
                // `${PROMPT_COMMAND:+; $PROMPT_COMMAND}` part preserves whatever the user
                // already had in PROMPT_COMMAND rather than clobbering it.
                "# Flush each command to .bash_history immediately instead of waiting for\n\
                 # the shell to exit, bruh's daemon polls this file on a timer and can\n\
                 # only see what's actually been written to disk.\n\
                 export PROMPT_COMMAND=\"history -a${PROMPT_COMMAND:+; $PROMPT_COMMAND}\""
                    .to_string(),
            )
        };

        let block = format!(
            "{}\n{}\n# Only start the daemon if one isn't already running. Without this\n\
             # check, every new terminal you open would spawn another daemon process,\n\
             # and they'd all end up polling the same files and flushing to the same\n\
             # Cognee dataset at the same time, which is exactly the kind of collision\n\
             # bruh's client-side retry logic exists to paper over, better to just not\n\
             # create the collision in the first place.\n\
             if ! pgrep -x \"bruh\" > /dev/null 2>&1; then\n\
             \x20   mkdir -p ~/.local/share/bruh\n\
             \x20   nohup bruh daemon > ~/.local/share/bruh/daemon.log 2>&1 &\n\
             fi\n{}\n",
            BLOCK_START, history_fix, BLOCK_END
        );
        write_managed_block(&profile, &block, force)
    }
}

// Shared append-or-replace logic for both the Windows and Unix branches above. If the
// marked block isn't present yet, we append it. If it is present and force is set, we cut
// out the old block (markers included) and splice the new one in at the same spot rather
// than just appending a second copy underneath. If it's present and force isn't set, we
// leave the file untouched, that's the normal "already done, nothing to do" case on a
// plain re-run of `bruh init`.
fn write_managed_block(
    profile: &std::path::Path,
    block: &str,
    force: bool,
) -> Result<Option<String>> {
    let existing = std::fs::read_to_string(profile).unwrap_or_default();
    let already_present = existing.contains(BLOCK_START);

    if already_present && !force {
        return Ok(None);
    }

    let new_content = if already_present {
        // Cut everything from BLOCK_START to BLOCK_END (inclusive) and splice the fresh
        // block in its place, so re-running with --force updates the content in place
        // instead of leaving a stale copy above a new one.
        //
        // The expect() here is safe: we only reach this branch when already_present is
        // true, and already_present is set from existing.contains(BLOCK_START) a few lines
        // up, so find() on the same string for the same substring is guaranteed to return
        // Some.
        let start = existing.find(BLOCK_START).expect("BLOCK_START must be present, already_present just confirmed existing.contains(BLOCK_START)");
        let end = existing
            .find(BLOCK_END)
            .map(|i| i + BLOCK_END.len())
            .unwrap_or(existing.len());
        format!(
            "{}{}{}",
            &existing[..start],
            block,
            &existing[end..].trim_start_matches('\n')
        )
    } else {
        // Fresh install: just tack it onto the end with a blank line separating it from
        // whatever the user already had, one newline of breathing room, nothing more. If
        // the file's empty or didn't exist yet, skip the separator so we don't leave a
        // couple of pointless blank lines at the top of a brand new .bashrc.
        let trimmed = existing.trim_end_matches('\n');
        if trimmed.is_empty() {
            format!("{}\n", block)
        } else {
            format!("{}\n\n{}", trimmed, block)
        }
    };

    let mut f =
        std::fs::File::create(profile).with_context(|| format!("Cannot write {:?}", profile))?;
    f.write_all(new_content.as_bytes())?;
    // Explicit flush even though File::write_all already goes straight to the OS without
    // any userspace buffering of its own, this is just making the intent obvious to
    // anyone reading the code later: we want this fully on disk before init reports
    // success, not queued up and dropped if the process gets killed a moment later.
    f.flush()?;

    Ok(Some(profile.to_string_lossy().to_string()))
}


--- ./src/cli/config_cli.rs ---
//! `bruh config list / get / set`, no hand-editing required by the user here.
//!
//! This is the thin CLI-facing wrapper around the actual Config logic living in cli/config.rs.
//! Config itself doesn't know or care about terminal formatting, colors, or how the CLI
//! arguments got parsed, this file's whole job is bridging "what the user typed" to "what
//! Config's methods expect" and then printing something nice back.

// We bring in the custom pretty printer
use crate::cli::{
    output::{bold, cyan, dim, green, print_footer, print_header},
    Config,
};
use anyhow::Result;

// Deliberately hand-maintained rather than reflecting over Config's fields at runtime
// (Rust doesn't give us that without extra crates for a project this size). The
// config_keys_cover_every_config_field test below is what actually keeps this honest: it
// serializes a real Config to JSON and asserts every field name shows up here too, so
// forgetting to add a new field here fails `cargo test` instead of just silently never
// appearing in `bruh config list`.
const DISPLAY_KEYS: &[&str] = &[
    "cognee_api_key",
    "cognee_api_url",
    "gemini_api_key",
    "groq_api_key",
    "claude_api_key",
    "llm_priority",
    "discovery_enabled",
    "discovery_rate_limit_seconds",
    "poll_interval_seconds",
    "batch_flush_interval_seconds",
    "max_buffer_size",
    "daemon_log_level",
    "offline_buffer_path",
    "excluded_commands",
];

/// Handles the `bruh config <sub>` family of subcommands.
///
/// `sub` is the subcommand (`"list"`, `"get"`, or `"set"`), already extracted from argv in
/// main.rs. `key` and `value` are `Option`s since `"list"` needs neither, `"get"` needs just
/// `key`, and `"set"` needs both.
///
/// # Errors
///
/// Returns an error if the config file can't be loaded, or if `key` doesn't match a known
/// configuration field.
pub fn run(sub: &str, key: Option<&str>, value: Option<&str>) -> Result<()> {
    match sub {
        "list" => {
            let cfg = Config::load()?;
            print_header("Configuration");
            println!();
            for k in DISPLAY_KEYS {
                // Get the configs and assign to each the indexes in the array of keys
                if let Some(v) = cfg.get_value(k) {
                    println!("  {}  {}", cyan(&format!("{:<35}", k)), bold(&v));
                }
            }
            println!();
            println!(
                "  Config file: {}",
                dim(&Config::config_path()?.to_string_lossy())
            );
            println!();
            print_footer();
        }
        "get" => {
            let k = key.ok_or_else(|| anyhow::anyhow!("Usage: bruh config get <key>"))?;
            let cfg = Config::load()?;
            match cfg.get_value(k) {
                Some(v) => println!("{}", v),
                None => anyhow::bail!("Unknown key '{}'. Run 'bruh config list'.", k),
            }
        }
        "set" => {
            let k = key.ok_or_else(|| anyhow::anyhow!("Usage: bruh config set <key> <value>"))?;
            let v = value.ok_or_else(|| anyhow::anyhow!("Usage: bruh config set <key> <value>"))?;
            let mut cfg = Config::load()?;
            // set_value does the actual parsing and validation (see cli/config.rs), we
            // just propagate any error with ? and only save to disk if it succeeded, so a
            // bad value never gets persisted over a working config.
            cfg.set_value(k, v)?;
            cfg.save()?;
            println!("  {} {} = {}", green("✓"), bold(k), v);
        }
        _ => {
            anyhow::bail!("Usage: bruh config <list|get|set> [key] [value]");
        }
    }
    Ok(()) // Satisfy the contract.
}

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes a real Config to JSON and checks its field names against DISPLAY_KEYS in
    // both directions: every JSON field should be listed here, and everything listed here
    // should be a real field. Whichever direction breaks tells you exactly what drifted,
    // add a field to Config and forget to list it here, or list a key that no longer (or
    // never did) exist on the struct.
    #[test]
    fn config_keys_cover_every_config_field() {
        let cfg = Config::default();
        let json = serde_json::to_value(&cfg).expect("Config should serialize");
        let fields = json.as_object().expect("Config serializes as an object");

        for field_name in fields.keys() {
            assert!(
                DISPLAY_KEYS.contains(&field_name.as_str()),
                "Config field '{}' exists on the struct but isn't in DISPLAY_KEYS, \
                 it'll never show up in `bruh config list`",
                field_name
            );
        }

        for key in DISPLAY_KEYS {
            assert!(
                fields.contains_key(*key),
                "DISPLAY_KEYS lists '{}' but Config has no such field anymore",
                key
            );
        }
    }

    #[test]
    fn every_display_key_resolves_through_get_value() {
        // The other half of the guarantee: being listed in DISPLAY_KEYS is only useful if
        // get_value() actually knows how to render it. This would have caught
        // offline_buffer_path and excluded_commands sitting on the struct with no display
        // support at all before this fix.
        let cfg = Config::default();
        for key in DISPLAY_KEYS {
            assert!(
                cfg.get_value(key).is_some(),
                "'{}' is in DISPLAY_KEYS but get_value() doesn't know how to render it",
                key
            );
        }
    }
}


--- ./src/cli/providers.rs ---
//! CLI-NEW-001: full cascade state, key presence, availability, priority, discovery counts.
// `bruh providers` is basically a dashboard for the discovery cascade. It answers three
// questions at a glance: which LLM providers have keys set, what order they'd be tried in
// (matching the cascade order in discovery/extractor.rs), and how many managers each one has
// actually discovered so far. Handy for figuring out why discovery picked a particular
// provider, or why it's not working at all.

use crate::{
    cli::{
        output::{bold, cyan, dim, green, orange, print_footer, print_header},
        Config,
    },
    discovery::{cache::load_learned_managers, extractor::PROVIDER_ORDER},
};
use anyhow::Result;

/// Which provider ProviderCascade::from_config would actually try first: the earliest
/// entry in the configured priority list that's available, falling back to PROVIDER_ORDER
/// for anything not explicitly ranked. Pulled out as its own function so it can be tested
/// directly against the exact ordering rules from_config uses, without needing to run the
/// whole `bruh providers` command end to end.
fn active_provider<'a>(llm_priority: &'a [String], available: &[&'a str]) -> Option<&'a str> {
    llm_priority
        .iter()
        .map(|s| s.as_str())
        .chain(PROVIDER_ORDER.iter().copied())
        .find(|id| available.contains(id))
}

/// Runs `bruh providers`, a dashboard of the LLM discovery cascade's configured and available state.
pub async fn run() -> Result<()> {
    let config = Config::load()?;

    print_header("LLM Provider Status");
    println!();

    // CONFIG-003: env_var here is just the label shown in the "set X to enable" hint, the
    // actual availability check below goes through Config::resolved_*_key() now, so a key
    // set via `bruh config set` shows up as available too, not just env vars.
    //
    // Display metadata (env var name, friendly label) for each provider in PROVIDER_ORDER.
    // This used to be its own separate array typing out "gemini"/"groq"/"claude" a second
    // time in a fixed order, now it's built by mapping over the single shared list instead.
    let display_meta = |id: &str| -> (&'static str, &'static str) {
        match id {
            "gemini" => ("GOOGLE_AI_API_KEY", "Gemini Flash"),
            "groq" => ("GROQ_API_KEY", "Groq (Llama-3)"),
            "claude" => ("ANTHROPIC_API_KEY", "Claude Haiku"),
            _ => ("", ""),
        }
    };

    // Count how many learned managers each provider discovered
    // This is purely cosmetic info, doesn't affect discovery behavior at all, just gives
    // the user a sense of "which provider has actually been doing the work" over time.
    let learned = load_learned_managers().unwrap_or_default();
    let mut discovery_counts = std::collections::HashMap::new();
    for profile in learned.values() {
        if let Some(ref p) = profile.discovered_by_provider {
            *discovery_counts.entry(p.clone()).or_insert(0usize) += 1;
        }
    }

    let mut available: Vec<&str> = Vec::new();

    // One row per provider, in PROVIDER_ORDER for the display itself (not the user's
    // configured priority order, that's shown separately below as its own line). We compute
    // each provider's position in the priority list just for the "primary" / "fallback #N"
    // label on each row.
    for id in PROVIDER_ORDER {
        let (env_var, display) = display_meta(id);
        let has_key = match *id {
            "gemini" => config.resolved_gemini_key().is_some(),
            "groq" => config.resolved_groq_key().is_some(),
            "claude" => config.resolved_claude_key().is_some(),
            _ => false,
        };
        let priority_pos = config.llm_priority.iter().position(|p| p == id);
        let priority_label = match priority_pos {
            Some(0) => bold("primary"),
            Some(n) => dim(&format!("fallback #{}", n + 1)),
            None => dim("not in priority list"),
        };
        let discoveries = discovery_counts.get(*id).copied().unwrap_or(0);

        if has_key {
            println!(
                "  {}  {:<20}  {}  [{}]  {} managers discovered",
                green("●"),
                bold(display),
                priority_label,
                dim(env_var),
                discoveries
            );
            available.push(id);
        } else {
            println!(
                "  {}  {:<20}  {}",
                orange("○"),
                display,
                dim(&format!("set {} or run bruh config set", env_var))
            );
        }
    }

    println!();

    // The active provider is whichever one ProviderCascade::from_config would actually try
    // first: the earliest entry in the user's llm_priority that has a key, falling back to
    // PROVIDER_ORDER for anything not explicitly ranked. This used to just take available[0]
    // from the fixed gemini/groq/claude display order, which meant a user with llm_priority
    // set to ["claude", "gemini", "groq"] would see this claim gemini was active even though
    // the cascade would genuinely try claude first.
    let active = active_provider(&config.llm_priority, &available);

    if let Some(active) = active {
        println!(
            "  {} Discovery is {} — active provider: {}",
            dim("→"),
            green("ENABLED"),
            bold(active)
        );
        println!(
            "  Priority order: {}",
            bold(&config.llm_priority.join(" → "))
        );
        println!();
        println!(
            "  Change priority: {}",
            dim("bruh config set llm_priority gemini,claude,groq")
        );
    } else {
        println!("  {} Discovery is {}", dim("→"), orange("DISABLED"));
        println!("  Configure at least one provider to enable package manager discovery.");
        println!();
        println!("  Free options:");
        println!("    Gemini:  {}", cyan("https://aistudio.google.com"));
        println!("    Groq:    {}", cyan("https://console.groq.com"));
    }

    println!();
    print_footer();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_provider_respects_configured_priority_over_display_order() {
        // This is the exact bug: all three have keys, but the user ranked claude first.
        // The old code took available[0] from the fixed gemini/groq/claude display order
        // and would have said "gemini" here, even though the cascade tries claude first.
        let priority = vec![
            "claude".to_string(),
            "gemini".to_string(),
            "groq".to_string(),
        ];
        let available = vec!["gemini", "groq", "claude"];
        assert_eq!(active_provider(&priority, &available), Some("claude"));
    }

    #[test]
    fn active_provider_falls_back_to_provider_order_for_unranked_keys() {
        // Only groq is available, and it isn't mentioned in llm_priority at all.
        let priority = vec!["claude".to_string()];
        let available = vec!["groq"];
        assert_eq!(active_provider(&priority, &available), Some("groq"));
    }

    #[test]
    fn active_provider_none_when_nothing_available() {
        let priority = vec!["claude".to_string(), "gemini".to_string()];
        let available: Vec<&str> = vec![];
        assert_eq!(active_provider(&priority, &available), None);
    }

    #[test]
    fn active_provider_skips_unavailable_higher_priority_entries() {
        // claude is ranked first but has no key, gemini is ranked second and does, gemini
        // should win.
        let priority = vec!["claude".to_string(), "gemini".to_string()];
        let available = vec!["gemini"];
        assert_eq!(active_provider(&priority, &available), Some("gemini"));
    }
}


--- ./src/cli/forget.rs ---
//! COGNEE-005: confirmation prompt before forgetting.
// The CLI side of the forget() operation. Since deleting memory permanently is the kind of
// thing you really don't want to accidentally trigger with a typo, this file's whole
// purpose is standing between the user and cognee::forget with a confirmation prompt,
// unless they've explicitly passed --force to skip it (handy for scripting or CI cleanup).

use crate::{
    cli::output::{bold, green, orange},
    cognee::forget,
};
use anyhow::Result;
use std::io::{self, Write};

/// Runs `bruh forget`, confirming with the user before deleting anything unless `force` is set.
pub async fn run(before: Option<String>, session: Option<String>, force: bool) -> Result<()> {
    // We require at least one of before/session, an unscoped "forget everything" felt too
    // dangerous to support as a bare default, better to make the user be explicit about
    // what they're deleting.
    if before.is_none() && session.is_none() {
        anyhow::bail!(
            "Specify what to forget:\n  bruh forget --before <date>\n  bruh forget --session <id>"
        );
    }

    println!();
    if let Some(ref b) = before {
        println!("  Will forget all events before: {}", bold(b));
        if session.is_none() {
            // COGNEE-009's note in cognee/forget.rs still applies: `before` isn't a
            // confirmed field in Cognee's public schema, and there's no documented
            // date-range delete. If the server silently ignores it the way it silently
            // ignored the wrong-cased field from COGNEE-016, this request could resolve to
            // "everything in the dataset" rather than the date-scoped subset you asked
            // for. Surfacing that here, right before the confirmation prompt, rather than
            // leaving it as a comment nobody reads until something's already gone.
            println!(
                "  {} This filter hasn't been confirmed against Cognee's actual schema yet.",
                orange("!")
            );
            println!(
                "    If it's silently ignored server-side, this could delete more than the date range above."
            );
            println!(
                "    Pairing this with {} scopes the request further and is the safer option.",
                bold("--session <id>")
            );
        }
    }
    if let Some(ref s) = session {
        println!("  Will forget session: {}", bold(s));
    }
    println!();

    // COGNEE-005: the actual confirmation gate. Plain stdin read rather than pulling in a
    // proper interactive-prompt crate, this only needs a yes/no and didn't feel worth the
    // extra dependency weight given the whole "avoid bloat for a hackathon build" approach
    // I mentioned back in main.rs.
    if !force {
        print!("  {} This cannot be undone. Continue? [y/N]: ", orange("!"));
        io::stdout().flush()?;
        let mut ans = String::new();
        io::stdin().read_line(&mut ans)?;
        if !matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  Cancelled.");
            return Ok(());
        }
    }

    forget(before, session).await?;

    println!("  {}  Memory entries removed.", green("✓"));
    Ok(())
}


--- ./src/cli/mod.rs ---
//! Every user-facing `bruh` subcommand, one submodule each.
//!
//! Every file declared below implements one of the commands listed in main.rs's help text,
//! with two exceptions: version and help aren't handled here at all. version reads its
//! version string and git hash straight out of the env! macro over in main.rs's own match
//! arm, and help is just main.rs's print_help() function firing on the catch-all arm. Both
//! answers live in main.rs itself, not in a submodule here, so don't go looking for them.
//! That leaves the eleven real commands below.

pub mod config;
pub mod config_cli;
pub mod explain;
pub mod forget;
pub mod improve;
pub mod init;
pub mod managers;
pub mod output;
pub mod providers;
pub mod query;
pub mod stats;
pub mod status;
pub mod watch;

pub use config::home_dir;
pub use config::Config;


--- ./src/cli/improve.rs ---
//! COGNEE-004: improve() with real, non-silent output.
// CLI wrapper for the improve command, this is what triggers Cognee's graph enrichment
// (their "memify" pass) over everything bruh has ingested so far. The "non-silent" bit in
// the doc comment above matters: this used to just run and print nothing, leaving the user
// staring at a blank terminal wondering if it had actually done anything, so I made sure it
// prints a clear header, a progress indicator, and a real summary line at the end.

use crate::{
    cli::output::{bold, dim, green, print_footer, print_header},
    cognee::improve,
};
use anyhow::Result;
use std::io::Write;

/// Runs `bruh improve`, triggering Cognee's graph enrichment pass with visible progress output.
pub async fn run() -> Result<()> {
    print_header("Memory Improvement");
    println!();
    print!("  Triggering Cognee memify / graph enrichment… ");
    std::io::stdout().flush()?;

    // false here: a person running `bruh improve` by hand wants to see it actually finish,
    // so this stays blocking. The daemon's own periodic trigger (daemon/mod.rs) is the one
    // that passes true and doesn't wait around, see the COGNEE-020 note in improve.rs.
    match improve(false).await {
        // The CLI only cares about the summary text, a person watching the terminal can
        // already see whether this succeeded from which match arm ran, the success flag is
        // for the daemon's own hourly summary log, see the COGNEE-023 note in improve.rs.
        Ok((_, summary)) => {
            println!("{}", green("✓"));
            println!();
            // Cognee doesn't always send back a text summary, its own response schema
            // treats that field as optional, so we fall back to a generic but still useful
            // message pointing the user toward `bruh stats` rather than printing nothing.
            if let Some(text) = summary {
                println!("  {}", bold("Improvement summary:"));
                // Capping at 20 lines so a huge summary doesn't scroll the terminal off
                // screen, the full detail is in Cognee's graph either way, this is just a
                // quick glance.
                for line in text.lines().take(20) {
                    println!("  {}", line);
                }
            } else {
                println!(
                    "  {}",
                    dim("Clustering complete. Run 'bruh stats' to see patterns.")
                );
            }
        }
        Err(e) => {
            println!();
            eprintln!(
                "  {}  Improvement failed: {}",
                crate::cli::output::orange("✗"),
                e
            );
            return Err(e);
        }
    }

    println!();
    print_footer();
    Ok(())
}


--- ./src/cli/query.rs ---
//! Handles three related things: the plain `bruh <query>` / `bruh query <text>` path, the
//! interactive `-i` REPL mode, and the `--raw` flag that switches between the friendly
//! timeline rendering and a raw JSON dump.

use crate::{
    cli::output::{cyan, dim, print_timeline},
    cognee::{recall, DATASET_NAME},
};
use anyhow::Result;
use std::io::{self, BufRead, Write};

// Let's talk through why this function exists, because it's answering a question that
// showed up in real testing. Someone typed "what's the name of the dataset we are
// communicating in now?" straight into bruh, and got back "the dataset name is
// unavailable" from Cognee. That looked like a routing bug, like bruh was asking the
// wrong dataset. It wasn't. recall() in cognee/query.rs already scopes every search to
// DATASET_NAME correctly. The real issue is that a question like this can never be
// answered from the memory graph, no matter which dataset you point it at, because the
// graph only knows what's inside the shell commands, git commits, and package installs
// we've fed it. Nothing in that content ever states "by the way, my own dataset name is
// X," so a GRAPH_COMPLETION search comes back empty every single time, correctly. The
// fix isn't to change what we search, it's to notice this is a question about bruh's own
// configuration, not about anything that happened in a terminal, and just answer it
// straight from the constant we already have, no round trip to Cognee needed at all.
fn local_dataset_answer(query: &str) -> Option<String> {
    let q = query.to_lowercase();
    let mentions_dataset = q.contains("dataset");
    let asking_what_or_which = q.contains("what") || q.contains("which") || q.contains("name");
    if mentions_dataset && asking_what_or_which {
        Some(format!(
            "We're communicating through the \"{}\" dataset. That's what the daemon writes \
             every shell command, git commit, and package install into, and it's the same \
             dataset every recall and explain query searches. This isn't something the \
             memory graph itself could ever tell you, since it only knows the activity \
             that's been recorded in it, not bruh's own configuration, so this answer comes \
             straight from bruh rather than from Cognee.",
            DATASET_NAME
        ))
    } else {
        None
    }
}

// query is the already-cleaned text (flags stripped, see parse_query_args in main.rs), and
// raw controls whether we print Cognee's response as-is (JSON) or render it as a friendly
// timeline via print_timeline.
/// Runs `bruh <query>` / `bruh query <text>`: a one-shot recall against Cognee, in timeline or raw JSON form.
pub async fn run(query: &str, raw: bool) -> Result<()> {
    // Meta-questions about bruh's own setup get answered here, locally, before we ever
    // touch the network. See local_dataset_answer's comment above for the full reasoning.
    if let Some(answer) = local_dataset_answer(query) {
        print_timeline(&serde_json::json!({ "text": answer }), raw);
        return Ok(());
    }

    // CLI-007: this used to print nothing at all while waiting on recall(). GRAPH_COMPLETION
    // queries can legitimately take a while (Cognee's own client integrations default to a
    // 5 minute timeout for this), and with zero feedback that just looked like a frozen
    // terminal. A one-line indicator makes it clear something's actually happening.
    eprint!("  {} ", dim("→ Thinking…"));
    io::stderr().flush()?;

    let response = recall(query).await?;
    eprint!("\r{}\r", " ".repeat(20)); // clear the "Thinking…" line before printing the real output
    print_timeline(&response, raw);
    Ok(())
}

/// "exit"/"quit" or their short forms "e"/"q" all end the interactive session.
fn is_exit_command(input: &str) -> bool {
    matches!(input, "exit" | "quit" | "e" | "q")
}

/// Only reachable via the `-i` / `--interactive` flag (see parse_query_args in main.rs).
/// Keeps prompting and querying until EOF (Ctrl-D), Ctrl-C, or the user types "quit",
/// "exit", or their short forms "q"/"e".
pub async fn run_interactive() -> Result<()> {
    // cyan()/dim()/orange() already no-op to plain text on their own when colors are
    // disabled (NO_COLOR, non-TTY, etc, see cli/output.rs), so there's no need to branch on
    // is_color_enabled() by hand here, one less thing to keep in sync.
    let prompt = format!("{} ", cyan("bruh>"));
    println!("bruh interactive mode. Press Ctrl-C or Ctrl-D to exit.\n");

    let stdin = io::stdin();
    loop {
        print!("{}", prompt);
        io::stdout().flush()?;

        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF (Ctrl-D)
            Ok(_) => {}
            Err(e) => {
                eprintln!("Read error: {}", e);
                break;
            }
        }

        let query = line.trim();
        if query.is_empty() {
            continue;
        }
        if is_exit_command(query) {
            break;
        }

        // Same local shortcut as the non-interactive path above, meta-questions about
        // bruh's own setup never need to leave the machine.
        if let Some(answer) = local_dataset_answer(query) {
            print_timeline(&serde_json::json!({ "text": answer }), false);
            continue;
        }

        // CLI-007: same "Thinking…" indicator as the non-interactive path, see run() above.
        eprint!("  {} ", dim("→ Thinking…"));
        io::stderr().flush()?;
        match recall(query).await {
            Ok(resp) => {
                eprint!("\r{}\r", " ".repeat(20));
                print_timeline(&resp, false)
            }
            Err(e) => {
                eprint!("\r{}\r", " ".repeat(20));
                eprintln!("{}  {}", crate::cli::output::orange("Error:"), e)
            }
        }
    }

    println!("\nGoodbye.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Let's cover the exact phrasing from the bug report first, then a couple of natural
    // variations of the same question, then make sure we're not so trigger-happy that we
    // swallow a real question that just happens to mention the word "dataset" in passing.
    #[test]
    fn test_catches_the_exact_reported_question() {
        let answer =
            local_dataset_answer("whats the name of the dataset we are communicating in now?");
        assert!(answer.is_some());
        assert!(answer.unwrap().contains(DATASET_NAME));
    }

    #[test]
    fn test_catches_common_phrasings() {
        assert!(local_dataset_answer("what dataset are we using").is_some());
        assert!(local_dataset_answer("which dataset is this").is_some());
        assert!(local_dataset_answer("dataset name?").is_some());
    }

    #[test]
    fn test_does_not_swallow_unrelated_questions() {
        // Mentions neither "dataset" nor the what/which/name combination, this should go
        // to Cognee like any other real question about actual activity.
        assert!(local_dataset_answer("what did I install yesterday").is_none());
        assert!(local_dataset_answer("show me my last git commit").is_none());
    }

    #[test]
    fn test_exit_command_recognises_all_four_forms() {
        assert!(is_exit_command("exit"));
        assert!(is_exit_command("quit"));
        assert!(is_exit_command("e"));
        assert!(is_exit_command("q"));
    }

    #[test]
    fn test_exit_command_does_not_swallow_real_queries() {
        assert!(!is_exit_command("explain my last error"));
        assert!(!is_exit_command("query something"));
        assert!(!is_exit_command(""));
    }
}


--- ./src/cli/config.rs ---
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
/// Resolves the user's home directory in a platform-aware way (Windows vs Unix env vars).
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
/// Every tunable setting `bruh` reads, serialized to and from `config.json`.
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
/// Loads the saved config from disk (or defaults if none exists yet), then layers any
    /// `BRUH_*` environment variable overrides on top.
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

    /// Writes the current config back out to `config.json`, creating the parent
    /// directory if needed.
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
    /// Path to `config.json`.
    pub fn config_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("config.json"))
    }
    /// Path to the discovery cache file, `learned_managers.json`.
    pub fn learned_managers_path() -> Result<PathBuf> {
        Ok(config_dir()?.join("learned_managers.json"))
    }
    /// The application's writable data directory (see the free [`data_dir`] function).
    pub fn data_dir() -> Result<PathBuf> {
        data_dir()
    }
    /// Path to the daemon's health snapshot file, `health.json`.
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
    /// The configured Gemini key, falling back to the `GOOGLE_AI_API_KEY` env var.
    pub fn resolved_gemini_key(&self) -> Option<String> {
        if !self.gemini_api_key.is_empty() {
            Some(self.gemini_api_key.clone())
        } else {
            std::env::var("GOOGLE_AI_API_KEY").ok()
        }
    }
    /// The configured Groq key, falling back to the `GROQ_API_KEY` env var.
    pub fn resolved_groq_key(&self) -> Option<String> {
        if !self.groq_api_key.is_empty() {
            Some(self.groq_api_key.clone())
        } else {
            std::env::var("GROQ_API_KEY").ok()
        }
    }
    /// The configured Claude key, falling back to the `ANTHROPIC_API_KEY` env var.
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
    /// Reads a config value by key for `bruh config get`/`list`, masking secrets as `<hidden>`.
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
    /// Sets a config value by key for `bruh config set`, parsing `value` into the field's
    /// real type and re-validating before returning.
    ///
    /// # Errors
    ///
    /// Returns an error if `key` is unrecognized, `value` fails to parse for that field's
    /// type, or the resulting config fails [`Config::validate`].
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


--- ./src/cli/watch.rs ---
//! CLI-007: `bruh watch <command>`, run a command, surface error memory on failure.
// `bruh watch cargo build` is the "wrap any command" helper: run it as normal, let stdout
// stream through live, but capture stderr, and if the command fails, immediately ask Cognee
// "have I seen this error before, and how did I fix it." This is meant to catch the exact
// moment you'd otherwise start googling an error you've already solved once.

use crate::{
    cli::{
        output::{bold, dim, exit_badge, print_watch_memory},
        Config,
    },
    cognee::recall,
    daemon::shell::{exclusion_patterns, is_excluded},
};
use anyhow::Result;
use std::{
    io::Write,
    process::{Command, Stdio},
};

/// Runs `bruh watch <command>`, running the command live and surfacing past-error memory on failure.
pub async fn run(cmd_args: &[String]) -> Result<()> {
    if cmd_args.is_empty() {
        anyhow::bail!("Usage: bruh watch <command> [args...]");
    }

    // split_first() only returns None for an empty slice, and the early return just above
    // guarantees cmd_args is non-empty by the time we get here, so this can't actually fail.
    let (program, rest) = cmd_args
        .split_first()
        .expect("cmd_args is non-empty, just checked by the is_empty() guard above");

    // Run the command, capturing stderr but passing stdout through.
    // stdout inherits the terminal directly so the wrapped command's normal output still
    // streams live as it would without watch. stderr gets piped instead, since that's the
    // half we actually need to inspect for error text, but we still echo it back out below
    // once we've captured it, so nothing visually disappears from the user's perspective.
    let mut child = Command::new(program)
        .args(rest)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to run '{}': {}", program, e))?;

    // Read stderr while the process runs
    // Reading stderr on its own thread rather than after child.wait() matters here, if the
    // child writes enough to stderr to fill the OS pipe buffer, and nobody's draining it,
    // the child can deadlock waiting for someone to read. Spawning this thread immediately
    // avoids that entirely, it just drains stderr into a String as the process runs.
    let stderr_handle = child.stderr.take();
    let stderr_text = std::thread::spawn(move || -> String {
        if let Some(mut stderr) = stderr_handle {
            let mut buf = String::new();
            use std::io::Read;
            let _ = stderr.read_to_string(&mut buf);
            buf
        } else {
            String::new()
        }
    });

    let status = child.wait()?;
    let stderr_output = stderr_text.join().unwrap_or_default();

    // Print captured stderr to terminal
    if !stderr_output.is_empty() {
        eprint!("{}", stderr_output);
    }

    if status.success() {
        return Ok(());
    }

    // Non-zero exit, query Cognee for error history
    let exit_code = status.code().unwrap_or(1);
    println!("\n{} exited {}", bold(program), exit_badge(exit_code));
    println!();

    // Build query from the error output (first meaningful error line)
    // Rather than shipping the entire stderr blob (which could be huge and mostly noise)
    // to Cognee, we pick out the first line that actually looks like an error message and
    // use just that as the search query, capped at 200 chars so we're not sending an
    // enormous prompt for what should be a quick lookup.
    let error_line = stderr_output
        .lines()
        .find(|l| {
            let l = l.to_lowercase();
            l.contains("error") || l.contains("failed") || l.contains("cannot")
        })
        .unwrap_or(&stderr_output)
        .trim()
        .chars()
        .take(200)
        .collect::<String>();

    if error_line.is_empty() {
        return Ok(());
    }

    // The exact same exclusion patterns that keep secrets out of the passive shell-history
    // poller (daemon::shell::is_excluded) get applied here too, before this text ever leaves
    // the machine as a recall() query. A failed authenticated curl, a `cat .env`, a stack
    // trace with a connection string, none of that should get a free pass just because it
    // came from watch's stderr capture instead of shell history.
    if let Ok(config) = Config::load() {
        let patterns = exclusion_patterns(&config);
        if is_excluded(&error_line, patterns) {
            println!(
                "  {} Skipping memory lookup, this error output matched an exclusion pattern.",
                dim("→")
            );
            return Ok(());
        }
    }

    print!("  {} Checking memory for similar errors… ", dim("→"));
    std::io::stdout().flush()?;

    let query = format!(
        "Have I seen this error before, and how did I fix it? Error: {}. \
         Show the fix commands if found, and how long it took to resolve.",
        error_line
    );

    match recall(&query).await {
        Ok(resp) => {
            println!("done\n");
            let text = extract_text(&resp);
            if text.trim().is_empty() || text.contains("No memory") || text.contains("not found") {
                println!("  {} No prior memory of this error.", dim("○"));
            } else {
                print_watch_memory("Prior fix found", &text);
            }
        }
        Err(e) => {
            // recall() is a direct HTTP call from this CLI process, it has nothing to do
            // with whether the background daemon is running, so a message that blames "the
            // daemon" here would point troubleshooting at the wrong thing. CogneeClient's
            // own errors are already specific (missing key, unreachable, auth failure), so
            // showing the real one is strictly more useful than guessing.
            println!("skipped ({})", e);
        }
    }

    Ok(())
}

fn extract_text(v: &serde_json::Value) -> String {
    for key in &["text", "result", "answer", "response"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    serde_json::to_string_pretty(v).unwrap_or_default()
}


--- ./src/cli/status.rs ---
//! CORE-003: bruh daemon --status, reads the health file written by the daemon.
//! CLI-NEW-001: bruh daemon --flush-now, forces a flush and resets backoff.
// This is deliberately a read-only, file-based status check rather than actually talking
// to the running daemon process (no IPC, no socket round trip). The daemon writes a fresh
// health.json every flush tick (see write_health() in daemon/mod.rs), so this just reads
// that snapshot and pretty-prints it. Simple, and it means `bruh daemon --status` works
// even if something's gone wrong enough that the daemon can't respond to a live query,
// as long as the file's still there from its last successful tick.

use crate::cli::{
    output::{bold, dim, fmt_datetime, fmt_time, green, orange, print_footer, print_header},
    Config,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

// A daemon in good health rewrites health.json every flush tick, so a snapshot older than
// a few flush intervals almost certainly means the process died without going through
// cleanup_sockets() (a hard kill, an OOM kill, a crash), not that it's just running quietly.
// Multiplying by 3 gives it enough slack to ride out one or two slow/failed flush cycles
// without crying wolf on a daemon that's actually fine.
const STALE_MULTIPLIER: u64 = 3;

/// CLI-NEW-001: force a flush by sending a signal file to the daemon.
/// This resets the backoff state and tells the daemon to attempt a flush on its next tick.
pub fn force_flush() -> Result<()> {
    let data_dir = Config::data_dir()?;
    let signal_path = data_dir.join("flush_now");
    
    // Create the directory if it doesn't exist
    if let Some(parent) = signal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    
    // Write the signal file with the current timestamp
    let timestamp = Utc::now().to_rfc3339();
    std::fs::write(&signal_path, timestamp)?;
    
    println!();
    println!("  {} Force flush signal sent to daemon.", green("✓"));
    println!("  {} Check status with: {}", dim("→"), bold("bruh daemon --status"));
    println!();
    
    Ok(())
}

/// Runs `bruh daemon --status`, reading and pretty-printing the daemon's last-written health file.
pub fn run() -> Result<()> {
    let health_path = Config::health_file_path()?;

    print_header("Daemon Status");
    println!();

    // No health file at all means either the daemon has never run, or it was killed hard
    // enough that cleanup_sockets() in daemon/mod.rs never got a chance to remove the old
    // file, either way, "not running" is the honest answer to give here since we have no
    // fresher information to go on.
    if !health_path.exists() {
        println!("  {} Daemon is not running.", orange("●"));
        println!();
        println!("  Start with: {}", bold("bruh daemon &"));
        println!();
        print_footer();
        return Ok(());
    }

    let content = std::fs::read_to_string(&health_path).unwrap_or_else(|_| "{}".into());
    let v: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();

    // Every field below is read defensively with .as_str()/.as_u64() plus an if-let, so a
    // partially written or slightly-out-of-date health.json (say, from an older bruh
    // version with fewer fields) just shows fewer status lines instead of erroring out.
    let status = v["status"].as_str().unwrap_or("unknown");

    // A daemon that died without a clean shutdown leaves its last real health.json behind
    // forever, since nothing else ever deletes it. Comparing "as_of" (written fresh on every
    // flush tick) against right now is how we tell that stale snapshot apart from a genuinely
    // live daemon, rather than trusting the file's mere existence at face value.
    let flush_interval = Config::load()
        .map(|c| c.batch_flush_interval_seconds)
        .unwrap_or(240);
    let stale_after = flush_interval.saturating_mul(STALE_MULTIPLIER);
    let is_stale = v["as_of"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|as_of| {
            let age = Utc::now().signed_duration_since(as_of.with_timezone(&Utc));
            age.num_seconds() > stale_after as i64
        })
        // No "as_of" at all (an older health.json written before this field existed) can't
        // be judged for freshness one way or the other, so we don't flag it as stale.
        .unwrap_or(false);

    let status_icon = match (status, is_stale) {
        ("running", false) => green("●"),
        _ => orange("●"),
    };

    let status_label = if is_stale {
        format!("{} (stale)", status)
    } else {
        status.to_string()
    };
    println!(
        "  {}  Status:            {}",
        status_icon,
        bold(&status_label)
    );

    if is_stale {
        println!(
            "  {}  {}",
            dim("│"),
            orange("Last update looks old, the daemon may have stopped responding.")
        );
        println!(
            "  {}  {}",
            dim("│"),
            orange("Try: bruh daemon --flush-now  or  restart the daemon")
        );
    }

    if let Some(checked_at) = v["as_of"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        println!(
            "  {}  Checked at:        {}",
            dim("│"),
            fmt_time(&checked_at.with_timezone(&Utc))
        );
    }

    if let Some(uptime) = v["uptime_seconds"].as_u64() {
        let h = uptime / 3600;
        let m = (uptime % 3600) / 60;
        let s = uptime % 60;
        println!("  {}  Uptime:            {}h {}m {}s", dim("│"), h, m, s);
    }

    if let Some(n) = v["events_queued"].as_u64() {
        println!("  {}  Events queued:     {}", dim("│"), n);
    }

    if let Some(n) = v["buffered_events"].as_u64() {
        let label = if n > 0 {
            orange(&n.to_string())
        } else {
            n.to_string()
        };
        println!("  {}  Buffered (offline):{}", dim("│"), label);
        
        // Show recommendation if buffer is getting large
        if n > 100 {
            println!(
                "  {}  {}",
                dim("│"),
                orange("Large buffer detected. Try: bruh daemon --flush-now")
            );
        }
    }

    if let Some(ts) = v["last_flush_time"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        println!(
            "  {}  Last flush:        {}",
            dim("│"),
            fmt_datetime(&ts.with_timezone(&Utc))
        );
    }

    if let Some(st) = v["last_flush_status"].as_str() {
        let formatted = if st == "success" {
            green(st)
        } else {
            orange(st)
        };
        println!("  {}  Flush status:      {}", dim("│"), formatted);
        
        // Show recommendation for failed flushes
        if st == "failed" {
            println!(
                "  {}  {}",
                dim("│"),
                orange("Check your Cognee API key with: bruh config get cognee_api_key")
            );
            println!(
                "  {}  {}",
                dim("│"),
                orange("Or try: bruh daemon --flush-now")
            );
        }
    }

    if let Some(backoff_secs) = v["backoff_seconds"].as_u64() {
        if backoff_secs > 0 {
            println!(
                "  {}  Backoff:           {} seconds remaining",
                dim("│"),
                backoff_secs
            );
            println!(
                "  {}  {}",
                dim("│"),
                orange("Flushes are paused. Try: bruh daemon --flush-now")
            );
        }
    }

    if let Some(n) = v["managers_known"].as_u64() {
        println!("  {}  Managers known:    {}", dim("│"), n);
    }

    if let Some(n) = v["managers_learned"].as_u64() {
        println!("  {}  Managers learned:  {}", dim("│"), n);
    }

    println!();
    print_footer();
    Ok(())
}


--- ./src/lib.rs ---
//! Cure your terminal of amnesia.
//!
//! `bruh` is a background daemon and CLI that watches your shell history, package manager
//! events, and git commits, then batches that activity off to [Cognee](https://www.cognee.ai/)'s
//! hybrid graph-vector memory so you can later ask plain-language questions about what you
//! were doing and why. Four operations sit at the center of it: remember (the daemon
//! batching and shipping events as they happen), recall (`bruh <query>`, asking the
//! accumulated memory a question), improve (asking Cognee to re-derive higher-level
//! structure over what's already been ingested), and forget (pruning a session or time
//! range back out).
//!
//! The [`daemon`] module owns the always-on background process, [`cli`] implements every
//! user-facing subcommand, [`discovery`] is the self-learning layer that figures out unknown
//! package managers on the fly, [`cognee`] is the thin client for the memory backend itself,
//! and [`events`] defines the shared event schema all of the above serialize through.
//!
//! This is the library root. Cargo picks this up automatically because it's named lib.rs,
// same way main.rs is picked up as the binary root. Having both means the project compiles
// as a library AND a binary, and the binary just pulls in the library crate. I did this
// mainly so tests/integration.rs (which lives outside src/) can actually reach into these
// modules. Without a lib.rs, integration tests would have no crate to import from.
//
// There used to be a blanket `#![allow(dead_code, unused_imports)]` right here. It's gone
// on purpose now. Blanket-allowing those two lints at the crate root silences them for
// every module, forever, which buries real signal (an orphaned helper, a forgotten wire-up)
// under the same umbrella as genuinely intentional unused code. Anything that truly needs
// to stay unused for now gets a scoped `#[allow(dead_code)]` right on that item with a
// comment explaining why, not a blanket pass for the whole crate.

// Same five modules as main.rs declares, just re-declared here for the library side.
// Yes it feels repetitive to list them twice (once here, once in main.rs) but that's just
// how the binary-plus-library crate pattern works in Rust. Small price to pay.
pub mod cli;
pub mod cognee;
pub mod daemon;
pub mod discovery;
pub mod events;


--- ./src/main.rs ---
// Thanks for opting to read through my code base

// THIS CODEBASE HAS VERY VERBOSE COMMENTS AS IT IS TAILORED FOR LEARNING PURPOSES

// I'll be explaining every line of code contextually while building.
// I hope this serves as a great learning point for you.

// There used to be a blanket #![allow(dead_code, unused_imports, unused_variables)] right
// here, added early on to keep the hackathon build quiet while things were still moving
// fast. It's gone now. The problem with silencing those lints crate-wide is that they're
// exactly the ones that catch a config value you parsed but forgot to wire up, or an old
// helper nobody calls anymore, real bugs, not just noise. Anything that legitimately needs
// to stay unused for now gets a scoped #[allow(...)] right on that item with a comment
// explaining why, so the rest of the crate keeps the safety net intact.

// Based on my project plan, these are the modules I'll primarily need for everything to be complete. I declare them using mod. If anything else comes up as I iterate, I'll add them here.
mod cli;
mod cognee;
mod daemon;
mod discovery;
mod events;

// At first I wanted using thiserror crate to experiment but I've come to realise I haven't used it much before now and there's no need taking such risk. I'll stay with anyhow for now.
use anyhow::Result;
use log::info;

// This is the part I have a lot to talk about. First of all, there's a crate called clap, that can handle commands for us quite neatly; I've built several projects with it but this one? I refuse to use it for this project because I'm operating from a quite inferior device. It usually takes long for heavy dependencies to be compiled and sometimes I experience crashes due to RAM shortages or so. To avoid meeting that issue towards the deadline, I'll have to handroll the Parser manually in the code. I'll also do that to several other crates that would otherwise make the project a bloatware. I know this doesn't matter to the final executable as Rustc will do all the optimizations but it's the best approach now for my convenience.

// This is a list of planned commands. I'll do some sort of matching later on but I keep it here as a reference to an array of string slices I can pull later on. It'll be a global constant.
const KNOWN_CMDS: &[&str] = &[
    "init",
    "daemon",
    "query",
    "stats",
    "forget",
    "improve",
    "managers",
    "providers",
    "config",
    "explain",
    "watch",
    "version",
    "--version",
    "-v",
    "--help",
    "-h",
];

// Commands that only ever take flags, never freeform positional text. Paired with each is
// the exact set of flags that command actually recognises. This exists to catch a real
// ambiguity in the natural-language shorthand: if someone types `bruh daemon seems stuck`
// without quoting it, the shell hands us ["daemon", "seems", "stuck"] as three separate
// words, and "daemon" alone is indistinguishable from someone genuinely typing the daemon
// subcommand. Since none of these commands ever expect stray positional words, seeing any
// is a strong signal the whole thing was meant as a query, not a subcommand invocation.
//
// config/managers/forget/watch/query are deliberately left out of this list: they
// legitimately take positional arguments as part of normal usage (a config key and value,
// a package manager name, a command to run), so "extra positional text" is expected and
// correct for them, not a sign of misrouting.
const FLAGS_FOR_BARE_CMD: &[(&str, &[&str])] = &[
    ("init", &["--force"]),
    ("daemon", &["--status", "--flush-now"]),
    ("stats", &[]),
    ("providers", &[]),
    ("explain", &[]),
    ("improve", &[]),
    ("version", &[]),
];

// True if every argument after the subcommand word is one of the flags that subcommand
// actually accepts (or there are no extra arguments at all). See FLAGS_FOR_BARE_CMD above
// for why this matters.
fn looks_like_bare_subcommand(args: &[String], allowed_flags: &[&str]) -> bool {
    args[2..]
        .iter()
        .all(|a| allowed_flags.contains(&a.as_str()))
}

// Looks for `flag` in args and returns whatever comes right after it. Used for anything
// that takes a value, --before, --session, --learn, instead of each command hand-rolling
// its own little index-walking loop to do the exact same thing.
fn extract_flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

// Just checks whether a bare flag like --force or --raw showed up anywhere in the args.
fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

// Pulls --raw and --interactive/-i out of a raw argument slice and returns the leftover
// words joined back into one query string, along with the two flag states. Both the
// shorthand `bruh "<query>"` path and the explicit `bruh query <text>` path call this now,
// so there's exactly one implementation of "how do we recognise flags in a query" instead
// of two that quietly did it differently: the shorthand path used to strip "--raw" with a
// plain string replace (which would mangle a query that happened to contain that substring
// as ordinary text) and never stripped --interactive/-i at all, so `bruh "my query"
// --interactive` silently didn't do what the equivalent `bruh query "my query"
// --interactive` did.
fn parse_query_args(args: &[String]) -> (String, bool, bool) {
    let raw = has_flag(args, "--raw");
    let interactive = has_flag(args, "--interactive") || has_flag(args, "-i");
    let text = args
        .iter()
        .filter(|a| !matches!(a.as_str(), "--raw" | "--interactive" | "-i"))
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    (text, raw, interactive)
}

// Rust functions are sync by default. To activate async, we apply the tokio::main 'attribute.'
#[tokio::main]
async fn main() {
    // PALETTE-001: left to its own devices, `-> Result<()>` on main() prints failures via
    // Rust's default Termination impl, which uses anyhow's Debug output ("Error: <chain>"),
    // plain and uncolored, that's exactly the flat text the earlier /recall timeout showed
    // up as. Wrapping the real body in run() and handling the Err case ourselves here means
    // every single error path in bruh, no matter how deep it propagates from, surfaces
    // through the same orange, human-readable formatting instead of Rust's raw default.
    if let Err(e) = run().await {
        eprintln!("{} {}", cli::output::orange("Error:"), e);
        // anyhow's error chain: each `.context()`/`with_context()` call up the stack adds
        // one link here, so a network failure three modules deep still shows its full story
        // instead of just the outermost, least specific message.
        for cause in e.chain().skip(1) {
            eprintln!("  {} {}", cli::output::dim("caused by:"), cause);
        }
        std::process::exit(1);
    }
}

// The actual program body, moved out of main() so the wrapper above can catch whatever
// error comes back and print it consistently instead of leaving that to Rust's default.
async fn run() -> Result<()> {
    // This initializes the env variables. On a normal day, I would have used dotenvy crate like in one of my past projects but this one feels much lighter for my environment.
    // Going forward, I'll use I, Me, My, We, Us, Ourselves and Our interchangeably as this codebase is for everyone and myself.
    // Quick story on this one, because it explains a real bug. Plain env_logger::init()
    // only ever looks at the RUST_LOG environment variable. Meanwhile daemon_log_level in
    // config.json gets parsed and saved just fine by cli/config.rs, but nobody ever wired
    // it into the actual logger, so it just sat there doing nothing. On top of that,
    // env_logger's default filter when RUST_LOG isn't set only lets error! calls through.
    // So every info!/warn!/debug! line in the daemon, things like "bruh daemon starting"
    // or "Flushing N events" or even a "Flush failed" warning, was getting silently
    // dropped. That's the whole reason daemon.log could sit there completely empty even
    // while the daemon was alive and doing real work. The fix below reads daemon_log_level
    // from config and hands it to env_logger as the default filter, so logging finally
    // reflects what the config file actually says. If someone has RUST_LOG set by hand,
    // we still respect that first, since that's a more explicit signal than the config.
    let log_level = std::env::var("RUST_LOG").unwrap_or_else(|_| {
        cli::Config::load()
            .map(|c| c.daemon_log_level)
            .unwrap_or_else(|_| "info".into())
    });
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(log_level)).init();
    // This is for us to receive arguments from the command line when cargo run, build or the command `bruh` is used. We collect the arguments in a Vector containing String values. Remember that I could have used the clap crate for all these but for memory.
    let args: Vec<String> = std::env::args().collect();
    // We then extract the index[1] command in the arguments to see if it matches any of the constant array of known commands we listed somewhere at the top of this file. // The actual first one(index[0]) is of course the command `bruh` but that doesn't belong to commands grouped as arguments here. In clap's terminology, we would say subcommand in certain instances.
    let first = args.get(1).map(|s| s.as_str());

    // Here, if index[1] is not a known command, we treat everything after the binary name as a natural-language query. Let me tell you briefly what led to this decision. I wanted something that sounded natural. At first, the design I had was something like `bruh query "what was the last thing I fixed?"` but then I realised we could just write `bruh "what was the last thing I fixed?"` without the query. This led to my checking for the first word.
    if let Some(f) = first {
        // Two ways this counts as a query instead of a real subcommand:
        //   1. The first word just isn't a known command at all (the common case, someone
        //      quoted their whole question like the docs show).
        //   2. The first word DOES match a known command, but it's one that only ever takes
        //      flags (see FLAGS_FOR_BARE_CMD), and there's stray non-flag text after it.
        //      That combination can only happen if someone typed an unquoted question that
        //      happens to start with a reserved word, like `bruh daemon seems stuck`, since
        //      a real invocation of that subcommand would never have extra freeform words
        //      dangling off the end.
        let is_reserved = KNOWN_CMDS.contains(&f);
        let misrouted_reserved_word = is_reserved
            && FLAGS_FOR_BARE_CMD
                .iter()
                .find(|(cmd, _)| *cmd == f)
                .is_some_and(|(_, flags)| !looks_like_bare_subcommand(&args, flags));

        if (!is_reserved && !f.starts_with('-')) || misrouted_reserved_word {
            let (query_clean, raw, interactive) = parse_query_args(&args[1..]);
            if interactive {
                return cli::query::run_interactive().await;
            }
            return cli::query::run(&query_clean, raw).await;
        }
    }

    // This is where we do a lot of file travelling. Apart from the return expression above, this is the main communication portal between the main file and every other file within the whole project.
    // It basically matches the index[1] we got all along to the proper module it belongs.
    // Recall that the "first" variable used the get method which returns an Option type, and that's why we're still using the extraction with the Some() variant.
    // In special cases where we have a flag or <option> to deal with, we will resolve that within the match arm itself.
    match first {
        Some("init") => {
            //  Note that --force reinstalls the git hook even if already present.
            if has_flag(&args, "--force") {
                cli::init::run_force()?; // Now here's the first time we're propagating with the question mark operator without counting the cli::query::run() above cause it will definitely return the Result Type by the end of the day.
            } else {
                cli::init::run()?; // This satisfies the init command
            }
        }

        Some("daemon") => {
            // The daemon is the first heart of this project. Without it, the project would be a recurring burden to developers

            // Just like we did with --force in init, we do same with daemon options for --status
            if has_flag(&args, "--status") {
                // This function will literally just tell us if the daemon is actively working or not then close the program(return).
                return cli::status::run().map_err(Into::into);
            }
            if has_flag(&args, "--flush-now") {
                return cli::status::force_flush().map_err(Into::into);
            }
            // Without the flag, it sets up the daemon.
            info!("Starting bruh daemon…");
            daemon::run().await?;
        }
        // Say a developer is oblivious of the natural language design, this is a provision for it to take the word "query" as an arguments
        // Wait, I just had an idea now!!! We could have a command where the user queries cognee for information then ports the response to an LLM with a custom prompt for something specific. That will be cool in future but eyes on the goal now.
        Some("query") => {
            // Same parse_query_args helper the shorthand path above uses, so both ways of
            // asking a question behave identically instead of two subtly different parsers.
            let (query_clean, raw, interactive) = parse_query_args(&args[2..]);

            if interactive {
                cli::query::run_interactive().await?;
            } else {
                // If there's nothing left after stripping flags, there's no actual query,
                // so we print a usage guide instead of sending an empty string to Cognee.
                // anyhow::bail! both builds the error and returns early in one step.
                if query_clean.is_empty() {
                    anyhow::bail!("Usage: bruh query <text>  |  bruh query --interactive");
                }
                cli::query::run(&query_clean, raw).await?;
            }
        }

        Some("forget") => {
            // Same extract_flag_value/has_flag helpers used everywhere else now, instead of
            // a hand-rolled loop walking the args by index.
            let before = extract_flag_value(&args, "--before");
            let session = extract_flag_value(&args, "--session");
            let force = has_flag(&args, "--force");
            cli::forget::run(before, session, force).await?;
        }

        //  Apart from being a daemon, this program will be self-learning. This means if it encounters a command it doesn't recognize (a package name), it will go online to search for what it is, figure it out and know it.
        // It probably will be able to do so on its own but this feature will be here in a situation where we want to force it or better said 'coerce' it to do so.
        // We initialize learn to nothing while expecting a String Type. Then we look for the --learn  flag. If we find it, we seek for the following argument and pass it to the managers::run associating function.
        Some("managers") => {
            // extract_flag_value handles the "flag with no value after it" case cleanly too
            // (args.get(i + 1) just returns None), no loop to get subtly wrong.
            let learn = extract_flag_value(&args, "--learn");
            cli::managers::run(learn).await?;
        }
        // The four wise men below are straightforward, aren't they?
        Some("stats") | Some("--stats") => {
            cli::stats::run().await?;
        }
        Some("providers") => {
            cli::providers::run().await?;
        }

        Some("explain") => {
            cli::explain::run().await?;
        }

        Some("improve") => {
            cli::improve::run().await?;
        }

        Some("watch") => {
            // We take everything after "watch" and pass it to the watch runner.
            // On the question left here before: args[2..] is a slice we're borrowing, not
            // something we own outright, so we can't move a String out of it directly. What
            // .to_vec() actually does is auto-ref that slice and clone every element into a
            // brand new, owned Vec<String>, it's a real (small) allocation and clone, not a
            // free reference. For a handful of CLI args that cost is nothing, and it's the
            // correct, idiomatic way to turn a borrowed slice into owned data you can hand
            // off to something else, like Command::args() below in watch.rs.
            let cmd_args = args[2..].to_vec();
            cli::watch::run(&cmd_args).await?;
        }
        // The config command needs awareness of three values.
        // We'll have a config list, set and get.
        // List goes in by default if none is provided
        // Set goes with no arguments key value pairs and get will just need the key to get the value according to the design.
        // The sub means subcommands and list goes in by default with no required arguments.
        Some("config") => {
            let sub = args.get(2).map(|s| s.as_str()).unwrap_or("list");
            let key = args.get(3).map(|s| s.as_str());
            let value = args.get(4).map(|s| s.as_str());
            cli::config_cli::run(sub, key, value)?;
        }

        // This helps users check the version of the package and git hash for it. It will be helpful in future for debugging and security verifications.
        Some("version") | Some("--version") | Some("-v") => {
            println!(
                "{} {} ({})",
                cli::output::bold("bruh"),
                env!("CARGO_PKG_VERSION"),
                cli::output::dim(env!("GIT_HASH"))
            );
        }
        // Let me point out something in this design. Remember that when the user does not use any of the command word or a flag, we take the string after the bruh command as a query. Now, the word 'help' is a very common verb and a lot of users will want to use it in their queries. For instance: `bruh help me check the last error`.If something like that happens, it will confuse the match parser and for that reason, we will not use the help option without the flag as commented out below. Anyone that needs help must use the flag or make some command errors.
        /*  Some("help") | */
        Some("--help") | Some("-h") | None => {
            print_help();
        }
        // This is where we catch-all. It's designed to be annoying when you get the commands wrong everytime. What we do is, whether the user calls the program with no arguments or with misunderstood queries, we print the error, and the help message. Then exit the program with a value that's NOT 0.
        Some(unknown) => {
            eprintln!("{} {}", cli::output::orange("Unknown command:"), unknown);
            print_help();
            std::process::exit(1);
        }
    }
    // Whew!!! That's it for the Parser. Now let's get the data flowing!

    Ok(()) // When the function ends successfully, it returns this to satisfy the return contract.
}

// We will print this basically when the program is run without valid arguments.
// It should be in a module of its own but it's not a crime here either.
// I know these are a lot of prints and it looks hazy but it's all good! I'm also aware of the memory cost of using the println macro though.
fn print_help() {
    use cli::output::{bold, cyan, dim, green};

    // One helper for the repeated "  command   description" row shape below, keeps every
    // line's spacing consistent without hand-aligning 15 different println! calls.
    let row = |cmd: &str, desc: &str| {
        println!("  {}  {}", green(&format!("{:<36}", cmd)), dim(desc));
    };

    println!(
        "{} {}\n",
        bold(&cyan("bruh")),
        dim("— persistent developer memory")
    );
    println!("{}", bold("USAGE:"));
    row("bruh <query>", "Natural language memory query (shorthand)");
    println!("  {} [options]\n", bold(&cyan("bruh <command>")));
    println!("{}", bold("COMMANDS:"));
    row("init", "Set up bruh (API keys, git hook, autostart)");
    row("daemon", "Start background daemon");
    row("daemon --status", "Show daemon health");
    row("daemon --flush-now", "Force a flush and reset backoff state");
    row("query <text> [--raw] [--interactive]", "Query memory");
    row("explain", "Session handoff brief for current directory");
    row(
        "watch <cmd> [args...]",
        "Run command; surface error history on failure",
    );
    row("stats", "Productivity summary");
    row("improve", "Trigger Cognee graph enrichment");
    row("forget --before <date>", "Forget events before date");
    row("forget --session <id> [--force]", "Forget a session");
    row("managers", "List known package managers");
    row("managers --learn <name>", "Learn a new package manager");
    row("providers", "Show LLM provider status");
    row("config list", "Show all configuration");
    row("config get <key>", "Get a config value");
    row("config set <key> <value>", "Set a config value");
    row("version", "Show version");
    println!();
    println!("{}", bold("ENV VARS:"));
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "BRUH_COGNEE_API_KEY")),
        dim("Override Cognee API key")
    );
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "BRUH_POLL_INTERVAL")),
        dim("Override poll interval (seconds)")
    );
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "NO_COLOR")),
        dim("Disable ANSI colors")
    );
    println!(
        "  {}  {}",
        cyan(&format!("{:<20}", "RUST_LOG")),
        dim("Log level (info, debug, warn)")
    );
}

// There are no other tests to run here beyond the ones right below. Let's see how it goes towards the end of the journey.
// I'll now go ahead to engage with each of the connected modules to ensure a round flow of data. Obviously, we'll start with the cli::query cause why not?

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn extract_flag_value_finds_the_value_after_the_flag() {
        let a = args(&["bruh", "forget", "--before", "2026-01-01"]);
        assert_eq!(
            extract_flag_value(&a, "--before"),
            Some("2026-01-01".to_string())
        );
    }

    #[test]
    fn extract_flag_value_missing_flag_is_none() {
        let a = args(&["bruh", "forget", "--force"]);
        assert_eq!(extract_flag_value(&a, "--before"), None);
    }

    #[test]
    fn extract_flag_value_flag_with_no_trailing_value_is_none() {
        // --learn as the very last argument, nothing after it to grab.
        let a = args(&["bruh", "managers", "--learn"]);
        assert_eq!(extract_flag_value(&a, "--learn"), None);
    }

    #[test]
    fn has_flag_detects_presence_and_absence() {
        let a = args(&["bruh", "forget", "--force"]);
        assert!(has_flag(&a, "--force"));
        assert!(!has_flag(&a, "--before"));
    }

    #[test]
    fn parse_query_args_strips_raw_as_a_token_not_a_substring() {
        // This is the exact bug that used to exist: a plain string .replace("--raw", "")
        // would mangle a query containing that substring anywhere, not just as a flag.
        let a = args(&["explain", "the", "--raw", "flag", "behavior"]);
        let (text, raw, interactive) = parse_query_args(&a);
        assert_eq!(text, "explain the flag behavior");
        assert!(raw);
        assert!(!interactive);
    }

    #[test]
    fn parse_query_args_strips_interactive_long_and_short_form() {
        let a = args(&["my", "query", "--interactive"]);
        let (text, _, interactive) = parse_query_args(&a);
        assert_eq!(text, "my query");
        assert!(interactive);

        let a = args(&["my", "query", "-i"]);
        let (text, _, interactive) = parse_query_args(&a);
        assert_eq!(text, "my query");
        assert!(interactive);
    }

    #[test]
    fn parse_query_args_plain_query_is_untouched() {
        let a = args(&["what", "was", "the", "last", "thing", "I", "fixed"]);
        let (text, raw, interactive) = parse_query_args(&a);
        assert_eq!(text, "what was the last thing I fixed");
        assert!(!raw);
        assert!(!interactive);
    }

    #[test]
    fn looks_like_bare_subcommand_true_for_no_extra_args() {
        let a = args(&["bruh", "daemon"]);
        assert!(looks_like_bare_subcommand(&a, &["--status"]));
    }

    #[test]
    fn looks_like_bare_subcommand_true_for_a_recognised_flag() {
        let a = args(&["bruh", "daemon", "--status"]);
        assert!(looks_like_bare_subcommand(&a, &["--status"]));
    }

    #[test]
    fn looks_like_bare_subcommand_false_for_stray_words() {
        // This is the unquoted-collision case: "daemon" is a real subcommand, but "seems"
        // and "stuck" aren't --status, so this was never really meant as the daemon command.
        let a = args(&["bruh", "daemon", "seems", "stuck"]);
        assert!(!looks_like_bare_subcommand(&a, &["--status"]));
    }

    #[test]
    fn reserved_word_collision_is_detected_for_bare_commands() {
        // Mirrors the exact check main.rs's shorthand-query branch does before dispatching.
        for word in ["daemon", "stats", "providers", "explain", "improve", "init"] {
            let a = args(&["bruh", word, "totally", "unrelated", "words"]);
            let f = a[1].as_str();
            let is_reserved = KNOWN_CMDS.contains(&f);
            assert!(is_reserved, "{word} should be a known command");
            let misrouted = FLAGS_FOR_BARE_CMD
                .iter()
                .find(|(cmd, _)| *cmd == f)
                .is_some_and(|(_, flags)| !looks_like_bare_subcommand(&a, flags));
            assert!(
                misrouted,
                "{word} followed by stray words should be treated as a query"
            );
        }
    }

    #[test]
    fn config_is_not_subject_to_the_bare_subcommand_check() {
        // config legitimately takes positional args (sub/key/value), so it's deliberately
        // absent from FLAGS_FOR_BARE_CMD, extra words after it are normal, expected usage.
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "config"));
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "managers"));
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "forget"));
        assert!(!FLAGS_FOR_BARE_CMD.iter().any(|(cmd, _)| *cmd == "watch"));
    }
}


--- ./src/cognee/ingest.rs ---
//! COGNEE-002: chunked batches (max 500 events per request).
// This is the write side of the cognee layer, everything that ends up in Cognee's memory
// graph flows through remember() at some point, whether that's the daemon's regular flush,
// a buffer replay after an outage, or a single git commit event sent immediately. The other
// three cognee submodules (query, improve, forget) are all reads or graph operations,
// this one's the only place we actually push new data in.

use super::CogneeClient;
use crate::events::Event;
use anyhow::{Context, Result};
use log::debug;

const CHUNK_SIZE: usize = 500;

/// COGNEE-019: this used to POST to "remember", which sounds like the obvious choice for
/// a function named remember(), but it isn't the right endpoint for what the daemon is
/// doing here. Cognee's own docs spell out what /api/v1/remember actually does under the
/// hood: add + cognify + (by default) improve, all run synchronously, in one blocking
/// call. cognify is the expensive part, it's the LLM-driven graph extraction step, and it
/// can legitimately take well over a minute on a growing dataset. Our daemon flushes on a
/// timer (batch_flush_interval_seconds), so calling /remember from the daemon meant every
/// single flush blocked the daemon's main loop for however long a full graph rebuild
/// happened to take that time.
///
/// Worse, Cognee computes the pipeline_run_id for a dataset deterministically (same user,
/// same dataset, same pipeline name always hashes to the same id), so if one flush's
/// cognify step was still running server-side when the next flush's timer fired, the
/// second call collided with the first under that same id. Cognee doesn't hand back a
/// clean "still busy" response for that, it surfaces as a plain 409 (see the COGNEE-018
/// note in cognee/mod.rs), which is the exact flakiness described in notes.txt.
///
/// /api/v1/add is the lower-level primitive /remember composes: pure ingest, no cognify,
/// no improve. It returns fast because there's no LLM graph-build attached to it at all.
/// The daemon now uses this for its regular ticking flush, and the actual graph-build
/// step is triggered separately and much less often by daemon::mod's improve trigger
/// (see COGNEE-020 there), so ingest cadence and graph-build cadence are no longer forced
/// to be the same number.
pub async fn remember(events: Vec<Event>) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    // COGNEE-013: shared client, see cognee/mod.rs. Avoids rebuilding a reqwest::Client
    // (and reloading Config from disk) on every remember() call.
    let client = CogneeClient::shared()?;

    let chunks: Vec<&[Event]> = events.chunks(CHUNK_SIZE).collect();
    let total_chunks = chunks.len();
    if total_chunks > 1 {
        debug!(
            "remember(): sending {} events across {} chunks of up to {}",
            events.len(),
            total_chunks,
            CHUNK_SIZE
        );
    }

    for (i, chunk) in chunks.into_iter().enumerate() {
        // Serialize events as structured text that Cognee's graph builder can process.
        let text_blocks: Vec<String> = chunk.iter().map(event_to_text).collect();

        // COGNEE-007: /api/v1/add (like /api/v1/remember before it) is multipart/form-data,
        // not JSON. Each text block goes in as a repeated "data" field, alongside the
        // dataset name the daemon writes activity history into.
        //
        // COGNEE-011: the "data" field is typed as UploadFile server-side, which
        // FastAPI only recognises when the multipart part has a filename in its
        // Content-Disposition header. Form::text() sends a plain field (name only)
        // and gets rejected with "Expected UploadFile, received: <class 'str'>".
        // Using Part::text().file_name(...) makes reqwest include the filename,
        // so the part is treated as an uploaded file instead of a form string.
        client
            .post_multipart("add", {
                let text_blocks = text_blocks.clone();
                move || {
                    let mut form =
                        reqwest::multipart::Form::new().text("datasetName", super::DATASET_NAME);
                    for (i, block) in text_blocks.iter().enumerate() {
                        let part = reqwest::multipart::Part::text(block.clone())
                            .file_name(format!("event_{i}.txt"))
                            .mime_str("text/plain")
                            .expect("static mime type is always valid");
                        form = form.part("data", part);
                    }
                    form
                }
            })
            .await
            // BUFFER-006: without this context, a chunk failure just surfaces as one
            // generic error for the whole call, no way to tell from the log which
            // chunk it was or how many had already gone through. On a big replay
            // (the offline buffer can hold thousands of events) that ambiguity is
            // exactly the kind of thing that leads to guessing instead of reading it
            // straight off the log.
            .with_context(|| {
                format!(
                    "chunk {}/{} failed ({} events in this chunk, {} sent successfully before it)",
                    i + 1,
                    total_chunks,
                    chunk.len(),
                    i * CHUNK_SIZE
                )
            })?;

        if total_chunks > 1 {
            debug!("remember(): chunk {}/{} sent", i + 1, total_chunks);
        }
    }
    Ok(())
}

/// Convenience wrapper around [`remember`] for sending a single event.
pub async fn remember_single(event: Event) -> Result<()> {
    remember(vec![event]).await
}

/// Convert an Event into a structured text block for Cognee ingestion.
fn event_to_text(event: &Event) -> String {
    match event {
        crate::events::Event::ShellCommand(e) => format!(
            "EVENT: shell_command\nTIMESTAMP: {}\nDIRECTORY: {}\nCOMMAND: {}\nEXIT_CODE: {}\nSESSION_ID: {}\nERROR_TYPE: {}\nOUTPUT: {}",
            e.timestamp.to_rfc3339(),
            e.directory,
            e.command,
            e.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()),
            e.session_id.as_deref().unwrap_or("unknown"),
            e.error_type.as_deref().unwrap_or("none"),
            e.output.as_deref().unwrap_or(""),
        ),
        crate::events::Event::PackageInstall(e) => format!(
            "EVENT: package_install\nTIMESTAMP: {}\nMANAGER: {}\nPACKAGE: {}\nVERSION: {}\nTRIGGER: {}\nSESSION_ID: {}\nDIRECTORY: {}",
            e.timestamp.to_rfc3339(),
            e.manager,
            e.package,
            e.version.as_deref().unwrap_or("unknown"),
            e.trigger_command.as_deref().unwrap_or("unknown"),
            e.session_id.as_deref().unwrap_or("unknown"),
            e.working_directory.as_deref().unwrap_or("unknown"),
        ),
        crate::events::Event::GitCommit(e) => format!(
            "EVENT: git_commit\nTIMESTAMP: {}\nHASH: {}\nMESSAGE: {}\nBRANCH: {}\nFILES: {}\nDIFF: {}\nSESSION_ID: {}\nDIRECTORY: {}",
            e.timestamp.to_rfc3339(),
            e.hash,
            e.message,
            e.branch,
            e.files_changed.join(", "),
            e.diff_summary.as_deref().unwrap_or(""),
            e.session_id.as_deref().unwrap_or("unknown"),
            e.working_directory.as_deref().unwrap_or("unknown"),
        ),
        crate::events::Event::PackageManagerProfile(p) => format!(
            "EVENT: package_manager_profile\nNAME: {}\nINSTALL_VERB: {}\nLIST: {}\nCONFIDENCE: {}\nPROVIDER: {}",
            p.name, p.install_verb, p.list_command, p.confidence,
            p.discovered_by_provider.as_deref().unwrap_or("unknown"),
        ),
    }
}



--- ./src/cognee/forget.rs ---
//! Sends the actual `forget` request to Cognee once the CLI's confirmation prompt clears it.

use super::CogneeClient;
use anyhow::Result;
use serde_json::json;

/// Deletes memory from Cognee, scoped by `session` and/or a `before` date cutoff.
///
/// This is the client-side half of `bruh forget`; [`crate::cli::forget`] handles the
/// interactive confirmation prompt before calling this.
///
/// # Errors
///
/// Returns an error if the request to Cognee fails.
pub async fn forget(before: Option<String>, session: Option<String>) -> Result<()> {
    // COGNEE-013: shared client, see cognee/mod.rs.
    let client = CogneeClient::shared()?;
    // Here's the thing this was missing. Every other cognee operation we make, add in
    // ingest.rs, cognify in improve.rs, and recall in query.rs, all point at the same
    // DATASET_NAME constant so they're all working with the same slice of data. This
    // function never did that, it left the dataset field out entirely. The COGNEE-009
    // note below already tells us the confirmed schema includes a dataset field, so
    // leaving it out isn't "no dataset," it's "whatever Cognee falls back to when you
    // don't say," which on a tenant that has more than one dataset is exactly the kind
    // of quiet mismatch that makes forget look like it did nothing, or worse, makes it
    // touch data outside what bruh actually owns. Referencing the shared constant here
    // brings forget in line with the other three, so the whole cognee layer is finally
    // reading the same one dataset name from the same one place.
    let mut body = json!({ "dataset": super::DATASET_NAME });
    // COGNEE-009: the confirmed /api/v1/forget schema is {dataset, session_id, memory_only, ...}.
    // `session` maps cleanly onto the documented session_id field. `before` (a date cutoff)
    // has no confirmed matching field in Cognee's public schema, there is no documented
    // date-range delete. It's passed through as-is so it isn't silently dropped, but this
    // needs verifying against the tenant's Swagger/OpenAPI page before relying on it.
    if let Some(b) = before {
        // Only warn in the specific risky combination: before with no session to also
        // scope the request. cli::forget's confirmation prompt already surfaces this risk
        // interactively, but --force skips that prompt entirely (by design, for
        // scripting), so this log line is the only trace left in that case. Once this
        // field's actually been verified against Cognee's schema, this warning (and the
        // one in cli/forget.rs) should come out.
        if session.is_none() {
            log::warn!(
                "forget --before '{}' used without --session, 'before' is not a confirmed \
                 Cognee schema field, if it's silently ignored server-side this may delete \
                 more than intended",
                b
            );
        }
        body["before"] = json!(b);
    }
    if let Some(s) = session {
        body["session_id"] = json!(s);
    }
    client.post("forget", body).await?;
    Ok(())
}


--- ./src/cognee/mod.rs ---
//! The thin HTTP client for Cognee's hybrid graph-vector memory API.
//!
//! `mod.rs` here is just the entry point: it declares the four operation submodules
//! (forget, improve, ingest, query) and re-exports their public functions, plus owns
//! [`CogneeClient`] itself, the shared, timeout-tuned `reqwest` client every operation
//! goes through.
//! This is the entry point for the cognee layer. As usual we first begin with the children declaration.

mod forget;
mod improve;
mod ingest;
mod query;
// We then call in the functions we need, yep, for public use via this API :-)
pub use forget::forget;
pub use improve::improve;
pub use ingest::{remember, remember_single};
pub use query::recall;

/// The single Cognee dataset every `bruh` operation reads from and writes to.
///
/// Every single thing bruh sends to or asks Cognee for (add, cognify, recall, forget) needs
// to point at the exact same dataset, otherwise you get precisely the situation that
// prompted this constant to exist: one part of the code writing into "bruh_activity" while
// another part quietly falls back to whatever Cognee's own default happens to be, and now
// your queries look like they know nothing even though the data's sitting right there.
// Before this, "bruh_activity" was typed out by hand in four separate files. That's four
// chances for a typo or a copy-paste slip to silently split bruh's memory across two
// datasets. One constant, referenced everywhere, means there's only one place to ever
// change it, and it's structurally impossible for ingest and recall to drift apart again.
pub const DATASET_NAME: &str = "bruh_activity";

// Every call into Cognee goes through anyhow::Result, so failures propagate with context
// attached instead of us hand-rolling a custom error enum for a project this size.
use anyhow::{Context, Result};
use serde_json::Value;
use std::{sync::OnceLock, time::Duration};

/// A configured HTTP client for Cognee's API, holding the API key, base URL, and a
/// connection-pooled `reqwest::Client` tuned with generous request and connect timeouts.
///
/// Most call sites should go through [`CogneeClient::shared`] rather than constructing
/// their own, so the whole process reuses one connection pool.
pub struct CogneeClient {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
}

/// COGNEE-013: a single process-wide CogneeClient instead of building a fresh one
/// (and a fresh reqwest::Client, meaning a fresh connection pool + TLS handshake)
/// on every remember()/recall()/improve()/forget() call. On a mobile connection
/// the handshake cost alone was a meaningful chunk of "why is this slow".
static SHARED_CLIENT: OnceLock<CogneeClient> = OnceLock::new();

/// COGNEE-014: GRAPH_COMPLETION-style queries route through an LLM over the graph
/// and can legitimately take a while, Cognee's own client integrations default to
/// a 5 minute timeout for exactly this reason. 30s (the old value) was cutting real
/// queries off mid-flight, which is what looked like "it just hangs, no reply".
/// remember() is comparatively fast, so it doesn't need this long a ceiling, but
/// giving every request the same generous timeout is simpler and safe, a request
/// that finishes in 2s doesn't wait around, this only bounds the worst case.
const REQUEST_TIMEOUT_SECS: u64 = 120;

// COGNEE-022: this used to be the only timeout, one 120s ceiling covering the whole
// request. That's the right budget for a slow-but-working GRAPH_COMPLETION call, but it
// meant a genuinely unreachable host (bad DNS, no route, a dead network on a background
// Termux process Android has throttled) also took the full 120 seconds to fail, every
// single time, on every single flush tick. A separate, much shorter timeout just for
// establishing the TCP+TLS connection lets "can't even reach the host" fail in seconds
// instead of minutes, while a request that DOES connect still gets the full 120s to
// actually respond. Two different failure modes, two different budgets.
const CONNECT_TIMEOUT_SECS: u64 = 15;

// We write custom functions for the CogneeClient struct
impl CogneeClient {
    // This creates a new instance of it. It accepts the api_key and api url while attempting to build the cliemt from the builder with a check of 30 seconds.
    /// Builds a new client from an explicit API key and URL.
    ///
    /// Prefer [`CogneeClient::shared`] or [`CogneeClient::from_config`] in most call sites.
    pub fn new(api_key: String, api_url: String) -> Self {
        Self {
            // We'll talk more abou this line
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_SECS))
                // COGNEE-013b: unwrap_or_default() used to silently fall back to a
                // bare reqwest::Client::default() with NO timeout at all if .build()
                // ever failed, meaning a request could hang forever with nothing
                // bounding it. .build() practically only fails on TLS backend
                // init issues, so if it does fail we want that to be loud, not a
                // silent switch to an unbounded client.
                .build()
                .expect("failed to build reqwest client (TLS backend init failed)"),
            api_key, // Struct Init Shorthand
            api_url, // Same here
        }
    }

    /// COGNEE-013c: returns the shared, process-wide client, building it from config
    /// on first use. Every cognee::* call site should go through this instead of
    /// CogneeClient::from_config() so the daemon (and any single CLI invocation that
    /// makes more than one call) reuses one connection pool.
    pub fn shared() -> Result<&'static CogneeClient> {
        if let Some(c) = SHARED_CLIENT.get() {
            return Ok(c);
        }
        let client = Self::from_config()?;
        // We ignore the Err from set() on purpose: if another thread raced us and set the
        // client first, that's fine, we don't care whose client instance actually won,
        // only that the OnceLock has something in it by the next line. Either way, .get()
        // immediately after is guaranteed to return Some.
        let _ = SHARED_CLIENT.set(client);
        Ok(SHARED_CLIENT
            .get()
            .expect("OnceLock has a value now regardless of whether set() above won the race"))
    }

    /// Builds a client from the on-disk/environment config.
    ///
    /// # Errors
    ///
    /// Returns an error if the config can't be loaded or no Cognee API key is set.
    pub fn from_config() -> Result<Self> {
        // We load values from the configuration file via the cli::Config::load function. Context is a method for error or more info for anyhow. This will return as a Struct we can access.
        let config = crate::cli::Config::load().context("Failed to load config")?;
        // We access to find if there is an api key for cognee otherwise we bail it. Remember strictly that the anyhow bail has a return implementation in it so the program will crash with the error message. In this scenario, we check if it's empty.
        if config.cognee_api_key.is_empty() {
            anyhow::bail!(
                "Cognee API key is not set.\n\
                 Run 'bruh init' to configure it, or set the BRUH_COGNEE_API_KEY environment variable.\n\
                 Get a key at: https://app.cognee.ai"
            );
        }
        // If it's not empty, we send the api key and api url to the new constructor function above, that will create a new CogneeClient and wrap it in Ok to be sent to the calling program.
        Ok(Self::new(config.cognee_api_key, config.cognee_api_url))
    }

    /// COGNEE-005: every real Cognee endpoint (self-hosted or Cognee Cloud) lives under
    /// the versioned `/api/v1/` prefix, e.g. `/api/v1/remember`, `/api/v1/recall`.
    /// Hitting the bare root (`{api_url}/{endpoint}`) 404s even against the correct host.
    fn build_url(&self, endpoint: &str) -> String {
        format!("{}/api/v1/{}", self.api_url.trim_end_matches('/'), endpoint)
    }

    /// COGNEE-006: Cognee Cloud tenants authenticate with `X-Api-Key: <key>`, not
    /// `Authorization: Bearer`. Bearer tokens are only for self-hosted instances after
    /// POST /api/v1/auth/login. We default to X-Api-Key since our default host
    /// (api.cognee.ai) and tenant subdomains (*.aws.cognee.ai) are both Cognee Cloud.
    fn auth_header_name(&self) -> &'static str {
        "X-Api-Key"
    }

    // COGNEE-015: was attempt < 2 (up to 2 retries = 3 attempts per call, each with
    // its own 1s/2s sleep). That was the *only* backoff in the system, do_flush()
    // and flush_buffered_events() each called through this on every tick with no
    // memory of prior failures, so a sustained outage meant every tick re-ran this
    // full retry ladder from scratch. Now that both flush paths check
    // buffer::should_retry() before attempting at all, this only needs to smooth
    // over brief blips, not carry an entire outage.
    //
    // COGNEE-018: bumped from 1 to 2. Here's why. A 409 from Cognee's pipeline registry
    // (see the 409 branch below) isn't always cleared up after a single short wait. If our
    // own call collided with a still-running cognify pipeline on the same dataset, that
    // pipeline might genuinely need another 10 to 30 seconds to finish, not 2. One extra
    // attempt, with a longer wait, gives a real, recoverable conflict a fair shot at
    // clearing before we give up and dump the batch into the offline buffer.
    const MAX_ATTEMPT_FOR_RETRY: u8 = 2;

    // Inspect an HTTP status and decide what the caller should do next.
    fn classify_status(status: reqwest::StatusCode, attempt: u8) -> StatusAction {
        if status.is_success() {
            StatusAction::Success
        } else if status.as_u16() == 401 || status.as_u16() == 403 {
            StatusAction::AuthError
        } else if status.is_server_error() && attempt < Self::MAX_ATTEMPT_FOR_RETRY {
            StatusAction::Retry
        } else if status.as_u16() == 409 && attempt < Self::MAX_ATTEMPT_FOR_RETRY {
            // COGNEE-018: this is the fix for the "409 errors" flakiness from notes.txt.
            // Cognee's own docs for /remember say plainly that it isn't a
            // PipelineRunErrored style 500 endpoint. Every failure inside it, including a
            // transient "there's already a pipeline run in progress for this dataset"
            // conflict, gets surfaced as a plain 409. Our pipeline_run_id for a given
            // dataset is deterministic (same user plus dataset plus pipeline name always
            // hashes to the same id), so if our daemon fires a second call while an
            // earlier one on the same dataset is still being processed server-side, we
            // collide with our own still-running job. That's not a real failure, it's bad
            // timing, and it clears up on its own once the earlier run finishes. Before
            // this fix we treated every 409 as instantly fatal (see the Fail branch
            // below), so a race we caused against ourselves looked identical to a genuine
            // error. Retrying gives the earlier run a chance to finish first.
            StatusAction::Retry
        } else {
            StatusAction::Fail
        }
    }

    /// Convenience wrapper around post_with_timeout using the shared client's default
    /// timeout. Almost everything (add, recall, forget) goes through this one, cognify is
    /// the sole exception that needs the longer timeout post_with_timeout allows for.
    pub async fn post(&self, endpoint: &str, body: Value) -> Result<Value> {
        self.post_with_timeout(endpoint, body, None).await
    }

    /// COGNEE-021: same as post(), but lets a caller ask for a longer timeout than the
    /// shared client's default 120s. Cognify is the one call in this whole file that
    /// genuinely needs this, Cognee's own docs mention it can take up to 10 minutes on a
    /// larger dataset, since it's an LLM chewing through everything, not a quick database
    /// write. Every other call (add, recall, forget) is fine with the normal client
    /// timeout, so this stays opt-in rather than raising the ceiling for everything.
    pub async fn post_with_timeout(
        &self,
        endpoint: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let url = self.build_url(endpoint);

        // Bounded by MAX_ATTEMPT_FOR_RETRY directly (0..=2, three attempts total) rather
        // than a separately hardcoded range. Before this, the loop bound and
        // MAX_ATTEMPT_FOR_RETRY were two different numbers that had to be kept in sync by
        // hand, drift apart and the unreachable!() below stops being unreachable. Deriving
        // one from the other removes that risk instead of just documenting it.
        for attempt in 0..=Self::MAX_ATTEMPT_FOR_RETRY {
            let mut req = self
                .client
                .post(&url)
                .header(self.auth_header_name(), &self.api_key)
                .header("Content-Type", "application/json")
                .json(&body); // Cognee accepts both strings and files so we send .json to it.

            if let Some(t) = timeout {
                req = req.timeout(t);
            }

            let resp = req.send().await.with_context(|| {
                format!(
                    "Network error reaching Cognee at {}.\n\
                     Check your internet connection or BRUH_COGNEE_API_URL.",
                    url
                )
            })?;
            let status = resp.status();
            match Self::classify_status(status, attempt) {
                StatusAction::Success => {
                    // We create some fallback if response isn't JSON
                    return resp
                        .json::<Value>()
                        .await
                        .or_else(|_| Ok::<_, anyhow::Error>(Value::Null));
                }
                StatusAction::AuthError => {
                    anyhow::bail!(
                        "Cognee API authentication failed (HTTP {}).\n\
                         Your API key may be invalid or expired.\n\
                         Run 'bruh config set cognee_api_key <new_key>' to update it.",
                        status
                    );
                }
                StatusAction::Retry => {
                    // COGNEE-018: a plain 2^attempt backoff makes sense for a genuine
                    // server error, we're just waiting out a hiccup. It doesn't make sense
                    // for a 409 pipeline conflict, because what we're actually waiting on
                    // is another pipeline run finishing, and that can take a good deal
                    // longer than 2 or 4 seconds. So 409s get their own, longer curve
                    // (10s, then 20s) instead of borrowing the network-hiccup one.
                    let wait = if status.as_u16() == 409 {
                        Duration::from_secs(10u64.saturating_mul(u64::from(attempt) + 1))
                    } else {
                        Duration::from_secs(2u64.pow(attempt as u32))
                    };
                    log::warn!("Cognee returned {}. Retrying in {:?}…", status, wait);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                StatusAction::Fail => {
                    let body_text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Cognee API error: HTTP {} — {}", status, body_text);
                }
            }
        }

        // Provably unreachable: classify_status only returns Retry (the one variant that
        // lets the loop continue) while attempt < MAX_ATTEMPT_FOR_RETRY, and the loop above
        // now never reaches an attempt value that high in the first place, since its bound
        // is MAX_ATTEMPT_FOR_RETRY itself.
        unreachable!()
    }

    /// COGNEE-007: /api/v1/remember is a multipart/form-data endpoint (it accepts raw
    /// text and/or file uploads plus batching form fields like chunks_per_batch), not
    /// a JSON body. This mirrors post() but sends a multipart::Form instead of .json().
    ///
    /// `build_form` is called fresh on every retry attempt since reqwest::multipart::Form
    /// is consumed on send and isn't Clone.
    pub async fn post_multipart<F>(&self, endpoint: &str, build_form: F) -> Result<Value>
    where
        F: Fn() -> reqwest::multipart::Form,
    {
        let url = self.build_url(endpoint);

        // Same reasoning as post_with_timeout's loop above: bound derived from
        // MAX_ATTEMPT_FOR_RETRY directly instead of a second hardcoded number that could
        // drift out of sync with it.
        for attempt in 0..=Self::MAX_ATTEMPT_FOR_RETRY {
            let resp = self
                .client
                .post(&url)
                .header(self.auth_header_name(), &self.api_key)
                .multipart(build_form())
                .send()
                .await
                .with_context(|| {
                    format!(
                        "Network error reaching Cognee at {}.\n\
                     Check your internet connection or BRUH_COGNEE_API_URL.",
                        url
                    )
                })?;

            let status = resp.status();
            match Self::classify_status(status, attempt) {
                StatusAction::Success => {
                    return resp
                        .json::<Value>()
                        .await
                        .or_else(|_| Ok::<_, anyhow::Error>(Value::Null));
                }
                StatusAction::AuthError => {
                    anyhow::bail!(
                        "Cognee API authentication failed (HTTP {}).\n\
                         Your API key may be invalid or expired.\n\
                         Run 'bruh config set cognee_api_key <new_key>' to update it.",
                        status
                    );
                }
                StatusAction::Retry => {
                    // COGNEE-018: see the matching comment in post() above, same reasoning
                    // applies here, remember/add go through this multipart path and a 409
                    // here is the exact same "another pipeline run on this dataset is
                    // still busy" conflict, so it gets the same longer backoff curve.
                    let wait = if status.as_u16() == 409 {
                        Duration::from_secs(10u64.saturating_mul(u64::from(attempt) + 1))
                    } else {
                        Duration::from_secs(2u64.pow(attempt as u32))
                    };
                    log::warn!("Cognee returned {}. Retrying in {:?}…", status, wait);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                StatusAction::Fail => {
                    let body_text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Cognee API error: HTTP {} — {}", status, body_text);
                }
            }
        }

        // Provably unreachable, same reasoning as post_with_timeout's loop above.
        unreachable!()
    }
}

enum StatusAction {
    Success,
    AuthError,
    Retry,
    Fail,
}


--- ./src/cognee/improve.rs ---
//! COGNEE-004: improve() returns a summary string for the CLI to display.
//!
//! COGNEE-021: this used to post to "/api/v1/improve", which doesn't exist on Cognee
//! Cloud at all. A live test against a real tenant confirmed a plain 404 on that path.
//! Cross-checking Cognee's own docs (the http-server router list, and the cognee-n8n
//! integration, which targets this exact kind of tenant) turned up the real verb:
//! Cognee Cloud's graph-build step is called "cognify", not "improve". "Improve" only
//! ever existed as a name inside the Python SDK's convenience wrapper around add plus
//! cognify, it was never an actual route on the wire. We're keeping bruh's own function
//! and CLI command named `improve` on purpose, since that's still a perfectly good name
//! for what this does from a user's point of view, we're just pointing it at the real
//! endpoint underneath.

use super::CogneeClient;
use anyhow::Result;
use serde_json::json;

/// COGNEE-020: added the `background` parameter. Before this, improve() always sent
/// `run_in_background: false`, meaning it always blocked until the whole graph enrichment
/// pass finished. That's exactly what you want when a person types `bruh improve` and is
/// sitting there watching the terminal, they want a real "done, here's your summary"
/// moment, not a fire-and-forget. But it's the wrong choice for the daemon's own periodic
/// trigger (see daemon/mod.rs), which needs to kick off enrichment on a timer without
/// blocking its main loop while an LLM chews through the graph.
///
/// COGNEE-021: the `background` flag no longer changes anything we send to Cognee itself.
/// The real cognify endpoint's body shape, confirmed against the docs, is just
/// `{"datasets": [...]}`, there's no documented flag for asking the server to run it
/// asynchronously. That's fine though, because the actual "don't block" behavior was
/// always coming from bruh's own side anyway: daemon/mod.rs already wraps this whole call
/// in `tokio::spawn(...)` before awaiting it, so the daemon's main loop never waits on
/// this function regardless of what we send. We keep the `background` parameter here so
/// neither call site (cli/improve.rs or daemon/mod.rs) needs to change, it's just now a
/// note to a future reader about which caller this is, rather than something we forward
/// over the wire.
/// COGNEE-023: returns an explicit `(succeeded, summary)` pair instead of just
/// `Result<Option<String>>`. Every failure path in this function already bails out early
/// via `?`, so on the Ok branch `succeeded` is always `true`, the only way to get a `false`
/// here would be a network/parse error, which is an `Err`, not an `Ok((false, _))`. That
/// makes the bool look redundant next to the Result, and in isolation it would be. The
/// reason to still carry it explicitly is the caller in daemon/mod.rs: it fires this off
/// via `tokio::spawn` and needs to record "did cognify succeed this hour" into a flag a
/// completely different part of the loop reads later for the hourly summary log. Matching
/// on Ok/Err to derive that bool works too, but it means the success/failure logic lives in
/// two places (Result's variant AND a match at the call site) instead of one value the
/// caller can just read off the tuple. cli/improve.rs, the other caller, ignores the flag
/// entirely and only cares about the summary text, which is exactly the asymmetry you'd
/// expect: one caller needs a machine-readable status, the other just prints a message.
pub async fn improve(background: bool) -> Result<(bool, Option<String>)> {
    // COGNEE-013: shared client, see cognee/mod.rs.
    let client = CogneeClient::shared()?;

    // COGNEE-021: the real endpoint is "cognify", and it wants an array of dataset
    // names, not a single "dataset_name" string. We still enrich the same dataset bruh
    // ingests activity into, using the shared DATASET_NAME constant, so this can never
    // drift from what ingest.rs writes to or query.rs reads from.
    let body = json!({
        "datasets": [super::DATASET_NAME],
    });

    log::info!(
        "Triggering cognify for dataset '{}' (background caller: {})…",
        super::DATASET_NAME,
        background
    );

    // COGNEE-021: 10 minutes, matching what Cognee's own docs say cognify can need on a
    // larger dataset. The shared client's normal 120s default is fine for add/recall/
    // forget, but would cut a genuinely slow cognify pass off mid-flight and make it
    // look like a failure when it might have just needed more time.
    let resp = client
        .post_with_timeout("cognify", body, Some(std::time::Duration::from_secs(600)))
        .await?;

    // Extract any summary text Cognee returns. Different response shapes have shown up
    // in practice, so we check a few plausible keys rather than betting on just one.
    let text = resp
        .get("text")
        .or_else(|| resp.get("result"))
        .or_else(|| resp.get("summary"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok((true, text))
}


--- ./src/cognee/query.rs ---
//! COGNEE-003: recall() with multi-format response parsing + fallback.

use super::CogneeClient;
use anyhow::Result;
use serde_json::{json, Value};

/// Asks Cognee a natural-language question over everything ingested into [`DATASET_NAME`](super::DATASET_NAME),
/// returning the parsed JSON response.
///
/// # Errors
///
/// Returns an error if the request to Cognee fails.
pub async fn recall(query: &str) -> Result<Value> {
    let client = CogneeClient::shared()?;
    // COGNEE-016: /api/v1/recall (confirmed against docs.cognee.ai/api-reference/recall/recall)
    // takes a *camelCase* body: searchType, datasets, datasetIds, systemPrompt, nodeName,
    // topK, onlyContext, verbose, unlike /api/v1/search's snake_case. We were sending
    // "search_type" (snake_case), which Cognee's Pydantic model just silently drops as an
    // unrecognised field and falls back to its own default (GRAPH_COMPLETION, so functionally
    // harmless by coincidence), but the real cost was that we sent no "datasets" field at all,
    // so every query searched whatever the tenant's default dataset is instead of
    // wherever remember() actually writes (see ingest.rs). Scoping to DATASET_NAME here,
    // the same constant ingest and improve use, is what makes recall() actually find what
    // bruh ingested instead of coming back empty against an unrelated dataset.
    let body = json!({
        "searchType": "GRAPH_COMPLETION",
        "query": query,
        "datasets": [super::DATASET_NAME],
    });
    let response = client.post("recall", body).await?;
    Ok(normalise_response(response))
}

/// Normalise various Cognee response shapes into a consistent `{ "text": "..." }` structure.
fn normalise_response(v: Value) -> Value {
    // COGNEE-017: the real /api/v1/recall (and /api/v1/search) response, confirmed against
    // docs.cognee.ai, is a JSON array of objects shaped like:
    //   [{ "search_result": <string|any>, "dataset_id": "...", "dataset_name": "..." }, ...]
    // This didn't match any of the shapes normalise_response used to check for ("text",
    // "result", "answer", "response", or array items with "text"/"content"), so a real
    // response always fell through to the raw pretty-printed JSON fallback, which is why
    // formatted output looked like a JSON dump instead of a clean timeline. Check this shape
    // first since it's the one Cognee actually sends.
    if let Some(arr) = v.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| item.get("search_result"))
            .filter_map(search_result_to_text)
            .filter(|s| !s.trim().is_empty())
            .collect();
        if !parts.is_empty() {
            return json!({ "text": parts.join("\n\n") });
        }
    }

    // Already has a "text" key, use it directly (kept as a fallback in case a
    // self-hosted tenant's response shape differs from Cognee Cloud's).
    if v.get("text").and_then(|t| t.as_str()).is_some() {
        return v;
    }

    // "result" key
    if let Some(s) = v.get("result").and_then(|t| t.as_str()) {
        return json!({ "text": s });
    }

    // "answer" key
    if let Some(s) = v.get("answer").and_then(|t| t.as_str()) {
        return json!({ "text": s });
    }

    // "response" key
    if let Some(s) = v.get("response").and_then(|t| t.as_str()) {
        return json!({ "text": s });
    }

    // Array of results with a "text"/"content" field (older/alternate shape)
    if let Some(arr) = v.as_array() {
        let combined: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        if !combined.is_empty() {
            return json!({ "text": combined.join("\n\n") });
        }
    }

    // null / empty → helpful placeholder
    if v.is_null() {
        return json!({ "text": "" });
    }

    // Unknown structure, pretty print as fallback
    json!({ "text": serde_json::to_string_pretty(&v).unwrap_or_default() })
}

/// `search_result` is typed `any` in Cognee's schema, usually a string for
/// GRAPH_COMPLETION, but other search_types can return nested objects/arrays.
fn search_result_to_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => serde_json::to_string_pretty(other).ok(),
    }
}


--- ./Cargo.toml ---
[package]
name = "bruh"
version = "0.1.0"
edition = "2021"
rust-version = "1.75"
authors = ["Obot Obo <oboobotenefiok@gmail.com>"]
description = "Cure Your Terminal Of Amnesia -- Background daemon and CLI tool for persistent developer memory"
license = "MIT OR Apache-2.0 OR Unlicense"
repository = "https://github.com/oboobotenefiok/bruh"
documentation = "https://docs.rs/bruh"
homepage = "https://github.com/oboobotenefiok/bruh"
readme = "README.md"
# crates.io caps a package at 5 keywords (and 20 characters each), so this is the
# short-list of what people would actually search for, not the full topic tag cloud.
keywords = ["cli", "daemon", "terminal", "memory", "ai"]
categories = ["command-line-utilities", "development-tools"]
# The packaged crate should ship source and docs, not the screenshots/diagrams used in the
# README. Those live under docs/images and are referenced by GitHub-relative links, they
# don't need to travel inside the .crate file that cargo install/cargo add download.
exclude = ["docs/images/*", "deep.sh", "deep.rs"]

[package.metadata.docs.rs]
all-features = true
rustdoc-args = ["--cfg", "docsrs"]

# Building as both a binary and a library (see src/main.rs and src/lib.rs) so the
# integration tests in tests/ can pull in the crate's modules through the normal public API
# instead of needing to be compiled as part of the binary itself.
[[bin]]
name = "bruh"
path = "src/main.rs"

[lib]
name = "bruh"
path = "src/lib.rs"

# Dependencies use caret ranges (Cargo's default) rather than exact "=" pins. Exact pins
# make sense for a binary you build and run yourself, but this crate is also a library:
# an exact pin here would forbid Cargo from unifying our version of, say, tokio or serde
# with whatever compatible version a downstream consumer's other dependencies need,
# breaking dependency resolution for anyone who depends on this crate alongside another.
# The committed Cargo.lock is what still gives *us* a reproducible `bruh` binary build;
# consumers of the library are free to resolve within the compatible range.
[dependencies]
# Async runtime for the daemon's event loop. We pull in the specific feature flags we
# actually use rather than the "full" bundle, keeps compile times down. macros for
# #[tokio::main], multi-thread for the runtime itself, io-util and net for the git socket,
# time for the poll/flush timers, signal for graceful shutdown, fs and process for the
# async-native filesystem and subprocess calls the daemon's pollers make on every tick
# (tokio::fs / tokio::process instead of std::fs / std::process::Command, so a slow disk
# read or a slow `pip list` can't block one of the runtime's worker threads and stall
# everything else scheduled on it, the git listener and shutdown signal check included).
tokio = { version = "1.35", features = ["macros", "rt-multi-thread", "io-util", "net", "time", "signal", "fs", "process"] }
# HTTP client for talking to Cognee and the LLM provider APIs. rustls-tls instead of the
# default openssl-based TLS backend so we don't need a system OpenSSL install, one less
# thing that can go wrong on a machine we don't control. "blocking" is included just for
# init.rs's synchronous API-key validation check, everything else uses the async client.
reqwest = { version = "0.11", features = ["json", "multipart", "rustls-tls", "blocking"], default-features = false }
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
# Timestamps everywhere in the event schema, with the "serde" feature so DateTime<Utc>
# fields serialize cleanly without us having to write custom (de)serializers.
chrono = { version = "0.4", features = ["serde"] }
# Lets ExtractorBackend (discovery/extractor.rs) be an async trait with dyn dispatch, which
# stable Rust doesn't support natively yet.
async-trait = "0.1"
anyhow = "1.0"
log = "0.4"
env_logger = "0.10"
# Used for SHELL-003's command exclusion patterns and a couple of small parsing spots.
regex = "1.10"
url = "2.4"
# Currently unused directly but kept as a direct dependency, some versions of our other
# deps expect indexmap in the tree and this keeps the lockfile stable across builds.
indexmap = "2.2"

[dev-dependencies]
# Only used by tests, gives us real temp files/dirs for the cursor and buffer round-trip
# tests instead of having to fake a filesystem.
tempfile = "3.8"


--- ./build.rs ---
//! This build.rs script embeds git commit info and a build timestamp into the binary at
//! compile time. Cargo runs any build.rs automatically before compiling the crate proper,
//! so by the time main.rs compiles, GIT_HASH and BUILD_TIMESTAMP are already available as
//! environment variables baked into the binary via env!(), which is how `bruh --version`
//! can tell you exactly which commit it was built from without needing git installed at
//! runtime. Worth noting, this file has to succeed for the crate to build at all, if it
//! panics, nothing else compiles.

use std::process::Command;

fn main() {
    // Ask git for the short commit hash, something like "a1b2c3d". If we're not in a git
    // repo, or git isn't installed, or anything else goes wrong, we fall back to "unknown"
    // rather than failing the build over a missing hash, a build without git metadata is
    // still a perfectly usable build.
    let hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|| "unknown".to_string());

    // cargo:rustc-env=KEY=VALUE is the magic string cargo watches for in build script
    // output, it sets an env var that env!("GIT_HASH") can read back at compile time in
    // the rest of the crate.
    println!("cargo:rustc-env=GIT_HASH={}", hash);

    // SOURCE_DATE_EPOCH is a convention some reproducible-build setups use to pin a fixed
    // timestamp so two builds of the same source produce byte-identical output. If it's
    // set, we just record "reproducible" instead of a real timestamp, since the whole
    // point of a reproducible build is that this field shouldn't vary between builds.
    // Otherwise we grab the actual current Unix timestamp.
    let ts = std::env::var("SOURCE_DATE_EPOCH")
        .ok()
        .and_then(|e| e.parse::<i64>().ok())
        .map(|_| "reproducible".to_string())
        .unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_else(|_| "unknown".to_string())
        });
    println!("cargo:rustc-env=BUILD_TIMESTAMP={}", ts);

    // By default cargo re-runs build.rs on every build. Telling it to only re-run when
    // .git/HEAD or the refs/heads directory change means we're not needlessly recomputing
    // this on every single `cargo build` when nothing about the commit has actually moved.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/refs/heads");
}


--- ./tests/integration.rs ---
//! TEST-006: integration tests for the event pipeline.
//! Uses the real parsing code with fixture files.
// This file lives outside src/ (in tests/) which is how Cargo knows to treat it as an
// integration test crate rather than a unit test module, it compiles against `bruh` as an
// external dependency, exactly the way a real consumer of the library would, using only
// the public API (that's why lib.rs exists at all, see the comment there). The focus here
// is the stuff that's genuinely worth testing end-to-end: serde round-tripping for every
// Event variant, since a schema mismatch there would silently corrupt data going to or
// coming from the offline buffer, plus the hashing and classification helpers that a lot
// of other logic depends on being correct.

// ── Shell history parser integration ─────────────────────────────────────────
// There used to be a zsh_fixture() helper here, a leftover from testing the shell parser
// against real fixture files early on, that never ended up used by anything below. Since
// daemon::shell is scoped pub(crate) rather than fully pub (see daemon/mod.rs's comment on
// why), an external crate like this one can't reach its parsing functions at all, so
// there's no fixture-based parser test this could ever back. Removed rather than kept
// around as unused, unused code that "might be handy later" is exactly the kind of thing
// that's easy to lose track of once it's just sitting there quietly.

/// Directly test the public-facing shell module logic via the events schema.
#[test]
fn test_command_hash_normalises_whitespace() {
    use bruh::events::command_hash;
    let h1 = command_hash("cargo  build  --release");
    let h2 = command_hash("cargo build --release");
    let h3 = command_hash("  cargo build --release  ");
    assert_eq!(h1, h2);
    assert_eq!(h2, h3);
}

#[test]
fn test_command_hash_differs_for_different_commands() {
    use bruh::events::command_hash;
    let h1 = command_hash("cargo build");
    let h2 = command_hash("cargo test");
    assert_ne!(h1, h2);
}

#[test]
fn test_classify_error_variants() {
    use bruh::events::classify_error;
    assert_eq!(
        classify_error("linker 'cc' not found"),
        Some("linker_error".into())
    );
    assert_eq!(
        classify_error("permission denied"),
        Some("permission_denied".into())
    );
    assert_eq!(
        classify_error("cannot find -lssl"),
        Some("missing_dependency".into())
    );
    assert_eq!(
        classify_error("error[E0499]: cannot borrow"),
        Some("compile_error".into())
    );
    assert_eq!(classify_error(""), None);
}

// ── NDJSON buffer integration ─────────────────────────────────────────────────
// These three round-trip tests each serialize an Event to JSON and deserialize it straight
// back, checking the fields survive the trip intact. This matters way more than it might
// look like at first glance, this exact serialize/deserialize path is what buffer.rs relies
// on to persist events to disk during a Cognee outage and read them back later, so a subtle
// serde bug here would mean silently losing or corrupting data exactly when the offline
// buffer is needed most.

#[test]
fn test_ndjson_round_trip() {
    use bruh::events::{Event, ShellCommandEvent};
    use chrono::Utc;

    let event = Event::ShellCommand(ShellCommandEvent {
        timestamp: Utc::now(),
        directory: "/tmp/test".into(),
        command: "cargo build".into(),
        exit_code: Some(0),
        output: None,
        duration_ms: Some(1234),
        session_id: Some("session_123".into()),
        command_hash: Some("abc123".into()),
        error_type: None,
    });

    let json = serde_json::to_string(&event).unwrap();
    let restored: Event = serde_json::from_str(&json).unwrap();

    match restored {
        Event::ShellCommand(e) => {
            assert_eq!(e.command, "cargo build");
            assert_eq!(e.session_id, Some("session_123".into()));
            assert_eq!(e.exit_code, Some(0));
        }
        _ => panic!("Wrong event variant"),
    }
}

#[test]
fn test_package_install_event_serde() {
    use bruh::events::{Event, ManagerType, PackageInstallEvent};
    use chrono::Utc;

    let event = Event::PackageInstall(PackageInstallEvent {
        timestamp: Utc::now(),
        manager: "apt".into(),
        manager_type: ManagerType::Bootstrapped,
        package: "libssl-dev".into(),
        version: Some("1.0.2".into()),
        trigger_command: Some("cargo build".into()),
        exit_code_trigger: Some(1),
        session_id: Some("session_456".into()),
        working_directory: Some("/home/user/project".into()),
    });

    let json = serde_json::to_string(&event).unwrap();
    assert!(json.contains("package_install"));
    assert!(json.contains("libssl-dev"));

    let restored: Event = serde_json::from_str(&json).unwrap();
    match restored {
        Event::PackageInstall(e) => {
            assert_eq!(e.package, "libssl-dev");
            assert_eq!(e.trigger_command, Some("cargo build".into()));
            assert_eq!(e.working_directory, Some("/home/user/project".into()));
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn test_git_commit_event_serde() {
    use bruh::events::{Event, GitCommitEvent};
    use chrono::Utc;

    let event = Event::GitCommit(GitCommitEvent {
        timestamp: Utc::now(),
        hash: "abc1234".into(),
        message: "fix: add libssl-dev".into(),
        branch: "main".into(),
        files_changed: vec!["Dockerfile".into()],
        session_id: Some("session_789".into()),
        working_directory: Some("/home/user/project".into()),
        diff_summary: Some("1 file changed, +1".into()),
    });

    let json = serde_json::to_string(&event).unwrap();
    let restored: Event = serde_json::from_str(&json).unwrap();
    match restored {
        Event::GitCommit(e) => {
            assert_eq!(e.hash, "abc1234");
            assert_eq!(e.diff_summary, Some("1 file changed, +1".into()));
            assert_eq!(e.working_directory, Some("/home/user/project".into()));
        }
        _ => panic!("Wrong variant"),
    }
}

#[test]
fn test_corrupt_ndjson_skipped() {
    // Simulates BUFFER-003: corrupt lines should be skippable. This calls the real
    // parse_buffer_lines function bruh's daemon actually uses, rather than a separate
    // filter_map here that would really just be re-testing serde_json's own error
    // behavior instead of bruh's skip-and-keep-going logic.
    let lines = vec![
        r#"{"event_type":"shell_command","timestamp":"2024-01-01T00:00:00Z","directory":"/","command":"ls","exit_code":0,"session_id":null,"command_hash":null,"error_type":null,"output":null,"duration_ms":null}"#,
        "this is not json at all",
        r#"{"event_type":"shell_command","timestamp":"2024-01-01T00:00:01Z","directory":"/","command":"pwd","exit_code":0,"session_id":null,"command_hash":null,"error_type":null,"output":null,"duration_ms":null}"#,
    ];
    let content = lines.join("\n");

    let (events, corrupt) = bruh::daemon::buffer::parse_buffer_lines(&content);

    // Corrupt line is skipped, 2 valid events pass through
    assert_eq!(events.len(), 2);
    assert_eq!(corrupt, 1);
}


