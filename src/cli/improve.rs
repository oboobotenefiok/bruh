//! COGNEE-004: improve() with real, non-silent output.
// CLI wrapper for the improve command, this is what triggers Cognee's graph enrichment
// (their "memify" pass) over everything bruh has ingested so far. The "non-silent" bit in
// the doc comment above matters: this used to just run and print nothing, leaving the user
// staring at a blank terminal wondering if it had actually done anything, so I made sure it
// prints a clear header, a progress indicator, and a real summary line at the end.

use crate::cli::output::{bold, dim, green, print_footer, print_header};
use crate::cognee::improve;
use anyhow::Result;
use std::io::Write;

pub async fn run() -> Result<()> {
    print_header("Memory Improvement");
    println!();
    print!("  Triggering Cognee memify / graph enrichment… ");
    std::io::stdout().flush()?;

    // false here: a person running `bruh improve` by hand wants to see it actually finish,
    // so this stays blocking. The daemon's own periodic trigger (daemon/mod.rs) is the one
    // that passes true and doesn't wait around, see the COGNEE-020 note in improve.rs.
    match improve(false).await {
        Ok(summary) => {
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
