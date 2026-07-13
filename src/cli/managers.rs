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

