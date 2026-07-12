// This is the discovery module's front door. Everything in here exists to answer one
// question: "I just saw a command for a package manager I've never heard of, what is it and
// how does it work?" That's the self-learning piece of the project I mentioned back in
// main.rs. The submodules below each handle one step of the pipeline.
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

