//! CLI-007: bruh watch <command>, run a command, surface error memory on failure.
// `bruh watch cargo build` is the "wrap any command" helper: run it as normal, let stdout
// stream through live, but capture stderr, and if the command fails, immediately ask Cognee
// "have I seen this error before, and how did I fix it." This is meant to catch the exact
// moment you'd otherwise start googling an error you've already solved once.

use crate::{
    cli::{
        output::{bold, dim, exit_badge, print_watch_memory},
        Config,
    },
    cognee::recall,
    daemon::shell::{exclusion_patterns, is_excluded},
};
use anyhow::Result;
use std::{
    io::Write,
    process::{Command, Stdio},
};

pub async fn run(cmd_args: &[String]) -> Result<()> {
    if cmd_args.is_empty() {
        anyhow::bail!("Usage: bruh watch <command> [args...]");
    }

    // split_first() only returns None for an empty slice, and the early return just above
    // guarantees cmd_args is non-empty by the time we get here, so this can't actually fail.
    let (program, rest) = cmd_args
        .split_first()
        .expect("cmd_args is non-empty, just checked by the is_empty() guard above");

    // Run the command, capturing stderr but passing stdout through.
    // stdout inherits the terminal directly so the wrapped command's normal output still
    // streams live as it would without watch. stderr gets piped instead, since that's the
    // half we actually need to inspect for error text, but we still echo it back out below
    // once we've captured it, so nothing visually disappears from the user's perspective.
    let mut child = Command::new(program)
        .args(rest)
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("Failed to run '{}': {}", program, e))?;

    // Read stderr while the process runs
    // Reading stderr on its own thread rather than after child.wait() matters here, if the
    // child writes enough to stderr to fill the OS pipe buffer, and nobody's draining it,
    // the child can deadlock waiting for someone to read. Spawning this thread immediately
    // avoids that entirely, it just drains stderr into a String as the process runs.
    let stderr_handle = child.stderr.take();
    let stderr_text = std::thread::spawn(move || -> String {
        if let Some(mut stderr) = stderr_handle {
            let mut buf = String::new();
            use std::io::Read;
            let _ = stderr.read_to_string(&mut buf);
            buf
        } else {
            String::new()
        }
    });

    let status = child.wait()?;
    let stderr_output = stderr_text.join().unwrap_or_default();

    // Print captured stderr to terminal
    if !stderr_output.is_empty() {
        eprint!("{}", stderr_output);
    }

    if status.success() {
        return Ok(());
    }

    // Non-zero exit, query Cognee for error history
    let exit_code = status.code().unwrap_or(1);
    println!("\n{} exited {}", bold(program), exit_badge(exit_code));
    println!();

    // Build query from the error output (first meaningful error line)
    // Rather than shipping the entire stderr blob (which could be huge and mostly noise)
    // to Cognee, we pick out the first line that actually looks like an error message and
    // use just that as the search query, capped at 200 chars so we're not sending an
    // enormous prompt for what should be a quick lookup.
    let error_line = stderr_output
        .lines()
        .find(|l| {
            let l = l.to_lowercase();
            l.contains("error") || l.contains("failed") || l.contains("cannot")
        })
        .unwrap_or(&stderr_output)
        .trim()
        .chars()
        .take(200)
        .collect::<String>();

    if error_line.is_empty() {
        return Ok(());
    }

    // The exact same exclusion patterns that keep secrets out of the passive shell-history
    // poller (daemon::shell::is_excluded) get applied here too, before this text ever leaves
    // the machine as a recall() query. A failed authenticated curl, a `cat .env`, a stack
    // trace with a connection string, none of that should get a free pass just because it
    // came from watch's stderr capture instead of shell history.
    if let Ok(config) = Config::load() {
        let patterns = exclusion_patterns(&config);
        if is_excluded(&error_line, patterns) {
            println!(
                "  {} Skipping memory lookup, this error output matched an exclusion pattern.",
                dim("→")
            );
            return Ok(());
        }
    }

    print!("  {} Checking memory for similar errors… ", dim("→"));
    std::io::stdout().flush()?;

    let query = format!(
        "Have I seen this error before, and how did I fix it? Error: {}. \
         Show the fix commands if found, and how long it took to resolve.",
        error_line
    );

    match recall(&query).await {
        Ok(resp) => {
            println!("done\n");
            let text = extract_text(&resp);
            if text.trim().is_empty() || text.contains("No memory") || text.contains("not found") {
                println!("  {} No prior memory of this error.", dim("○"));
            } else {
                print_watch_memory("Prior fix found", &text);
            }
        }
        Err(e) => {
            // recall() is a direct HTTP call from this CLI process, it has nothing to do
            // with whether the background daemon is running, so a message that blames "the
            // daemon" here would point troubleshooting at the wrong thing. CogneeClient's
            // own errors are already specific (missing key, unreachable, auth failure), so
            // showing the real one is strictly more useful than guessing.
            println!("skipped ({})", e);
        }
    }

    Ok(())
}

fn extract_text(v: &serde_json::Value) -> String {
    for key in &["text", "result", "answer", "response"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    serde_json::to_string_pretty(v).unwrap_or_default()
}
