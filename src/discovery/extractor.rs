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

