//! COGNEE-005: confirmation prompt before forgetting.
// The CLI side of the forget() operation. Since deleting memory permanently is the kind of
// thing you really don't want to accidentally trigger with a typo, this file's whole
// purpose is standing between the user and cognee::forget with a confirmation prompt,
// unless they've explicitly passed --force to skip it (handy for scripting or CI cleanup).

use crate::cli::output::{bold, green, orange};
use crate::cognee::forget;
use anyhow::Result;
use std::io::{self, Write};

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
