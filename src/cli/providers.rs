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

