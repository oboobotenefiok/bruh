//! CLI-NEW-001: full cascade state, key presence, availability, priority, discovery counts.
// `bruh providers` is basically a dashboard for the discovery cascade. It answers three
// questions at a glance: which LLM providers have keys set, what order they'd be tried in
// (matching the cascade order in discovery/extractor.rs), and how many managers each one has
// actually discovered so far. Handy for figuring out why discovery picked a particular
// provider, or why it's not working at all.

use crate::cli::output::{bold, cyan, dim, green, orange, print_footer, print_header};
use crate::cli::Config;
use crate::discovery::cache::load_learned_managers;
use anyhow::Result;

pub async fn run() -> Result<()> {
    let config = Config::load()?;

    print_header("LLM Provider Status");
    println!();

    // CONFIG-003: env_var here is just the label shown in the "set X to enable" hint, the
    // actual availability check below goes through Config::resolved_*_key() now, so a key
    // set via `bruh config set` shows up as available too, not just env vars.
    let providers = [
        ("gemini", "GOOGLE_AI_API_KEY", "Gemini Flash"),
        ("groq", "GROQ_API_KEY", "Groq (Llama-3)"),
        ("claude", "ANTHROPIC_API_KEY", "Claude Haiku"),
    ];

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

    // One row per provider, in the fixed display order above (not the user's configured
    // priority order, that's shown separately below). We compute each provider's position
    // in the priority list just for the "primary" / "fallback #N" label.
    for (id, env_var, display) in &providers {
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

    // Summary line at the bottom: either "discovery is fully off, here's how to enable it"
    // or "discovery is on, here's what would actually get used right now" (which is just
    // the first available provider in priority order, matching ProviderCascade's behavior).
    if available.is_empty() {
        println!("  {} Discovery is {}", dim("→"), orange("DISABLED"));
        println!("  Configure at least one provider to enable package manager discovery.");
        println!();
        println!("  Free options:");
        println!("    Gemini:  {}", cyan("https://aistudio.google.com"));
        println!("    Groq:    {}", cyan("https://console.groq.com"));
    } else {
        let active = available[0];
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
    }

    println!();
    print_footer();
    Ok(())
}
