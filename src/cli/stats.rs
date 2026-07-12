//! CLI-005: real productivity summary from Cognee.
// `bruh stats` asks Cognee to summarize everything it knows about you into a structured
// productivity snapshot: how many commands, how many packages, commit count, session count,
// what tends to go wrong, how fast you fix it, when you're most active. We're leaning on
// the LLM behind recall() to do the actual aggregation rather than computing these numbers
// ourselves from raw events, since Cognee's graph already has the full picture and doing it
// there means we don't need to duplicate that logic locally.

use crate::{cli::output::print_stats_box, cognee::recall};
use anyhow::Result;

// The exact keys build_stats_table() below looks for. Kept as one list so the prompt and
// the parser can't quietly drift apart the way they could if these were just typed out
// twice.
const STATS_FIELDS: &[(&str, &str)] = &[
    ("Total commands", "commands"),
    ("Packages installed", "packages_installed"),
    ("Git commits", "git_commits"),
    ("Sessions", "sessions"),
    ("Most common error", "most_common_error"),
    ("Avg fix time", "avg_fix_time"),
    ("Most productive hour", "most_productive_hour"),
    ("Longest session", "longest_session"),
];

pub async fn run() -> Result<()> {
    // Earlier this just described the fields we wanted in prose ("Include: total commands,
    // packages installed by manager, ...") and hoped the model would happen to answer back
    // with matching JSON. It sometimes did and sometimes didn't, since nothing in the prompt
    // actually told it to respond in JSON at all, so the clean table path below was mostly
    // going unused and falling back to raw text instead. Spelling out the exact keys and
    // saying "respond with only JSON" gives the model a format it can't really misread,
    // so the table renders far more often.
    let keys = STATS_FIELDS
        .iter()
        .map(|(_, key)| *key)
        .collect::<Vec<_>>()
        .join(", ");
    let prompt = format!(
        "Give me a structured summary of all developer activity. Respond with a single \
         JSON object and nothing else, no prose before or after it, using exactly these \
         keys: {}. Use your best estimate for any value you can't determine precisely, \
         omit a key entirely rather than guessing wildly if you have no signal for it at all.",
        keys
    );

    let response = recall(&prompt).await?;
    let text = extract_text(&response);
    render_stats_table(&text);
    Ok(())
}

/// Render a clean table from the stats response.
fn render_stats_table(text: &str) {
    let json = extract_json(text);

    if let Some(lines) = build_stats_table(&json) {
        print_stats_box(&lines);
    } else {
        // No structured data found, fall back to raw text inside the same box styling
        // the rest of the CLI uses, rather than a bare, uncolored dump.
        crate::cli::output::print_header("Developer Activity Report");
        println!();
        print_raw_stats(text);
        println!();
        crate::cli::output::print_footer();
    }
}

/// Pulls whichever of the known fields are actually present in the parsed JSON into the
/// (label, value) pairs print_stats_box() wants. Returns None if nothing matched at all, so
/// the caller knows to fall back to raw text instead of printing an empty box.
fn build_stats_table(json: &serde_json::Value) -> Option<Vec<(&'static str, String)>> {
    if json.is_null() {
        return None;
    }

    let mut lines = Vec::new();
    for (label, key) in STATS_FIELDS {
        if let Some(val) = json.get(key) {
            let s = match val {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            lines.push((*label, s));
        }
    }

    if lines.is_empty() {
        None
    } else {
        Some(lines)
    }
}

/// Fallback: print raw text when JSON parsing fails.
fn print_raw_stats(text: &str) {
    for line in text.lines().take(30) {
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            println!("  {}", trimmed);
        }
    }
}

/// Extract JSON from the LLM response, handling prose wrapping.
fn extract_json(text: &str) -> serde_json::Value {
    if let Some(start) = text.find('{') {
        if let Some(end) = text.rfind('}') {
            if let Ok(v) = serde_json::from_str(&text[start..=end]) {
                return v;
            }
        }
    }
    serde_json::Value::Null
}

/// Extract the raw text from a Cognee response, checking common fields.
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

