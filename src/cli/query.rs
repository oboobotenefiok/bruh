// First the trailing argument query mode (handled in main.rs).
// Then the interactive query mode.
// And the  --raw flag.
// Then the formatted timeline output.

use crate::cli::output::{cyan, dim, print_timeline};
use crate::cognee::{recall, DATASET_NAME};
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

// Remember from the main file that query run needed two things
// 1. the cleaned up query itself and
// 2. the state of the raw flag whether true or false.
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

    // Now this looks really simple doesnt it? We're calling cognee and saying, HEY!!! Do you remember something like this?
    let response = recall(query).await?;
    eprint!("\r{}\r", " ".repeat(20)); // clear the "Thinking…" line before printing the real output
                                       // We then pass it to the cli::ouput which will print the response with the given format of raw or not. Now that's easy right!!! // That's how easy it is to use cognee!!!!!!!!!!!!!!!!!!!
                                       // We don't have to worry about how they manage the vector this, graph that and all of that.
    print_timeline(&response, raw);
    // The unit type is passed wrapped in the Ok variant for the result type contract of the function to be satisfied. This will be passed to the main function at the line where I said would rather have been the first line with the question mark operator. The one with the await and what were we awaiting? Yep, right! Cognee's response. The reason for the async function we have here.
    Ok(())
}

/// This is only available when you use the query command with the -i flag. It keeps querying until EOF or Ctrl-C OR you type quit or exit. I'll add q and e soon.
pub async fn run_interactive() -> Result<()> {
    // cyan()/dim()/orange() already no-op to plain text on their own when colors are disabled
    // (NO_COLOR, non-TTY, etc, see cli/output.rs), so there's no need for us to branch on
    // is_color_enabled() by hand here anymore like the old code did, one less thing to keep in sync.
    let prompt = format!("{} ", cyan("bruh>"));
    // This tells the user the state he's in and how to escape it.
    println!("bruh interactive mode. Press Ctrl-C or Ctrl-D to exit.\n");
    // We then place ourselves in a loop.

    let stdin = io::stdin();
    loop {
        // This prints the prompt we built above.
        print!("{}", prompt);
        io::stdout().flush()?;
        // We declare a new line to listen to in the match.
        let mut line = String::new();
        match stdin.lock().read_line(&mut line) {
            Ok(0) => break, // EOF when triggered.
            Ok(_) => {}     // Every other thing does nothing
            Err(e) => {
                // An error will get printed and the loop broken
                eprintln!("Read error: {}", e);
                break;
            }
        }
        // Then we check if query is an empty value, otherwise...
        // We trim and pass line to query for that reason.
        // I feel there can be more idiomatic Rust for all these
        let query = line.trim();
        if query.is_empty() {
            continue;
        }
        // ...we break the loop if exit or quit is typed.
        // I'll implement for e and q later but this is just as fine.
        if query == "exit" || query == "quit" {
            break;
        }
        // We call the cognee recall and pass the query to it just like in the non-interactive mode.
        // We handle error here quite differently though. We match the response instead of propagating with the ? operator.
        // You'd definitely want to do same if you were me cause now we're in INTERACTIVE MODE.
        // Same local shortcut as the non-interactive path above, meta-questions about bruh's own
        // setup never need to leave the machine.
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

    println!("\nGoodbye."); // :-)
    Ok(()) // As usual, we satisfy the contract.
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
}
