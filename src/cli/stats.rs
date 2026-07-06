//! CLI-005: real productivity summary from Cognee.
// `bruh stats` asks Cognee to summarize everything it knows about you into a structured
// productivity snapshot: how many commands, how many packages, commit count, session count,
// what tends to go wrong, how fast you fix it, when you're most active. We're leaning on
// the LLM behind recall() to do the actual aggregation rather than computing these numbers
// ourselves from raw events, since Cognee's graph already has the full picture and doing it
// there means we don't need to duplicate that logic locally.

use crate::cli::output::{print_footer, print_header};
use crate::cognee::recall;
use anyhow::Result;
use prettytable::{Attr, Cell, Row, Table};

pub async fn run() -> Result<()> {
    // This prompt is deliberately specific about the exact fields we want back, matching
    // the keys build_stats_table looks for below, so if the LLM cooperates we get clean
    // structured output rather than having to scrape numbers out of free-form prose.
    let response = recall(
        "Give me a structured summary of all developer activity. \
         Include: total commands, packages installed by manager, git commits, \
         sessions count, most common errors, average fix time in seconds, \
         most productive hour of day, longest session duration.",
    )
    .await?;

    // Extract text from response
    let text = extract_text(&response);

    // Render as a proper table
    render_stats_table(&text);

    Ok(())
}

/// Render a clean table from the stats response.
fn render_stats_table(text: &str) {
    print_header("Developer Activity Report");
    println!();

    let json = extract_json(text);

    // If we have structured data, render a table
    if !json.is_null() {
        let mut table = Table::new();

        // Header row with bold style
        let header_row = Row::new(vec![
            Cell::new("Metric").with_style(Attr::Bold),
            Cell::new("Value").with_style(Attr::Bold),
        ]);
        table.add_row(header_row);

        // Data rows in a specific order
        let fields = [
            ("Total commands", "commands"),
            ("Packages installed", "packages_installed"),
            ("Git commits", "git_commits"),
            ("Sessions", "sessions"),
            ("Most common error", "most_common_error"),
            ("Avg fix time", "avg_fix_time"),
            ("Most productive hour", "most_productive_hour"),
            ("Longest session", "longest_session"),
        ];

        let mut has_data = false;
        for (label, key) in &fields {
            if let Some(val) = json.get(key) {
                let s = match val {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                table.add_row(Row::new(vec![
                    Cell::new(label),
                    Cell::new(&s),
                ]));
                has_data = true;
            }
        }

        if has_data {
            // Print the table
            table.printstd();
        } else {
            // No structured data found, fall back to raw text
            print_raw_stats(text);
        }
    } else {
        // No JSON found, fall back to raw text
        print_raw_stats(text);
    }

    println!();
    print_footer();
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
