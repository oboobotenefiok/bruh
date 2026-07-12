//! CLI-006: bruh explain, session handoff brief for the current directory.
// This is my favorite command in the whole project honestly. The idea: you sit down at
// your desk, cd into a project you haven't touched in a few days, run `bruh explain`, and
// it hands you back a plain-language brief of what you were doing last, without you having
// to scroll back through shell history or git log yourself trying to remember. It's really
// just a specially crafted recall() prompt scoped to the current directory, all the real
// intelligence is Cognee's, we're just asking the right question.

use crate::{
    cli::output::{dim, print_explain},
    cognee::recall,
};
use anyhow::Result;
use std::io::Write;

pub async fn run() -> Result<()> {
    let cwd = std::env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "unknown".into());

    // Same "show something while we wait" pattern as cli/query.rs's Thinking indicator,
    // GRAPH_COMPLETION queries aren't instant and a silent terminal makes it look stuck.
    print!("  {} Scanning memory for {}… ", dim("→"), dim(&cwd));
    std::io::stdout().flush()?;

    // Build a targeted recall query scoped to this directory
    // This prompt is doing a lot of work here, it's explicitly telling Cognee's graph
    // completion what shape of answer we want (last session's work, most recent commit,
    // packages, errors, unfinished business) and how to format it, rather than just
    // asking a vague "what happened here" and hoping for something coherent back.
    let query = format!(
        "Generate a developer context brief for the project in directory '{}'. \
         Include: what was being worked on last session, the most recent git commit, \
         any packages installed, errors encountered, and what was left unfinished. \
         Format as a concise handoff brief a developer would read before resuming work.",
        cwd
    );

    let response = recall(&query).await?;
    println!("done\n");

    let narrative = extract_narrative(&response);
    print_explain(&cwd, &narrative);

    Ok(())
}

// recall() already runs its own normalise_response (see cognee/query.rs), but I keep this
// second, slightly different extraction here as a belt-and-suspenders fallback since
// explain's use case is a bit more sensitive to getting SOME readable text back rather than
// silently failing, worst case we just pretty-print the raw JSON rather than showing
// nothing at all.
fn extract_narrative(v: &serde_json::Value) -> String {
    for key in &["text", "result", "answer", "response", "content"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    serde_json::to_string_pretty(v).unwrap_or_else(|_| "No context available.".into())
}

