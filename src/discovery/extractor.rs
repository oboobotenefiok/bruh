//! DISCOVERY-005: extract_with_cascade_verbose for CLI --learn output.
//! DISCOVERY-008: per-provider prompt optimisation.
// This file is the brain of the cascade idea. Instead of hardcoding "call Gemini, and if
// that fails call Groq" as an if/else chain, I built a small trait (ExtractorBackend) that
// all three providers implement identically, then I loop over them in priority order. This
// means adding a fourth provider later is just: write a struct that implements the trait,
// add it to the list in ProviderCascade::from_config, done. No branching logic to touch.

use crate::cli::Config;
use crate::discovery::providers::{ClaudeBackend, GeminiBackend, GroqBackend};
use crate::events::PackageManagerProfile;
use anyhow::Result;
use async_trait::async_trait;
use log::{info, warn};

// The contract every LLM backend has to satisfy. name() is just for logging so we know who
// answered, is_available() lets us skip providers whose API key isn't set without wasting a
// network round trip, and extract() is the actual work.
#[async_trait]
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
pub struct ProviderCascade {
    backends: Vec<Box<dyn ExtractorBackend>>,
}

impl ProviderCascade {
    // Builds the cascade in the order the user configured (config.llm_priority), falling
    // back to whatever order is left for any providers they didn't explicitly rank. I use a
    // HashMap here as scratch space just so I can remove() entries as I place them into the
    // ordered Vec, that way nothing gets duplicated and nothing gets dropped.
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

        // Append any providers not in the priority list
        // Whatever's left in the map (providers the user didn't mention in their config)
        // still gets appended at the end, so we never silently drop a working provider just
        // because the user forgot to list it.
        backends.extend(map.into_values());
        Self { backends }
    }

    // Walks the cascade in order and returns the first successful extraction. If a backend
    // isn't available (no API key) we skip it without even trying. If it IS available but
    // the call fails (rate limited, bad JSON, network hiccup, whatever) we log a warning and
    // just move on to the next one rather than giving up immediately. Only if every single
    // backend fails do we bail with an actionable error message telling the user which env
    // vars to set.
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
