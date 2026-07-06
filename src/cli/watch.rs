//! CLI-007: bruh watch <command>, run a command, surface error memory on failure.
// `bruh watch cargo build` is the "wrap any command" helper: run it as normal, let stdout
// stream through live, but capture stderr, and if the command fails, immediately ask Cognee
// "have I seen this error before, and how did I fix it." This is meant to catch the exact
// moment you'd otherwise start googling an error you've already solved once.

use crate::cli::output::{bold, dim, orange, print_watch_memory};
use crate::cognee::recall;
use anyhow::Result;
use std::io::Write;
use std::process::{Command, Stdio};

pub async fn run(cmd_args: &[String]) -> Result<()> {
    if cmd_args.is_empty() {
        anyhow::bail!("Usage: bruh watch <command> [args...]");
    }

    let (program, rest) = cmd_args.split_first().expect("checked above");

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
    println!(
        "\n{} exited with code {}",
        bold(program),
        orange(&exit_code.to_string())
    );
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
        Err(_) => {
            // Fail silently, watch is still useful even without memory lookup
            println!("skipped (daemon not running?)");
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
