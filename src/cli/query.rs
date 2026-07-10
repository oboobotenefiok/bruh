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
