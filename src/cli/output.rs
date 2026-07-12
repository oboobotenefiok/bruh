//! CLI-004: Timeline output with ANSI colors and local time conversion.
//! CLI-NEW-002: UTC timestamps displayed as local time.
//! POLISH-007: NO_COLOR / TERM=dumb support.

use chrono::{DateTime, Local, Utc};

// ANSI escape codes, inline so we need zero extra dependencies.
//
// PALETTE-001: bruh's whole terminal look runs on three colors on purpose, not the usual
// red/yellow/green traffic light set. Green and cyan are close cousins on the color wheel,
// so they read as "the same family" even when they're marking different things (green for
// success, cyan for structure), and deep orange is the one color that's warm enough to grab
// attention for warnings and errors without being the alarm-red every other CLI already
// uses. Plain terminal white barely shows up at all, most body text just gets no color code
// added and rides on whatever the user's terminal default foreground is, which keeps things
// legible on both light and dark terminal themes instead of us guessing wrong.
//
// Deep orange isn't one of the 16 standard ANSI colors, there's no escape code for "deep
// orange" in the basic set, so this reaches into the 256-color palette instead (code 166,
// a burnt/rust orange that's dark enough to stay readable on both light and dark
// backgrounds, unlike the very bright default "orange-ish" 208 which can wash out on light
// terminals). Every terminal from the last ~15 years supports 256-color mode, so this is a
// safe bet.
const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[2m";
const GREEN: &str = "\x1b[32m";
const CYAN: &str = "\x1b[36m";
const DEEP_ORANGE: &str = "\x1b[38;5;166m";

pub fn is_color_enabled() -> bool {
    use std::io::IsTerminal;

    if std::env::var("NO_COLOR").is_ok() {
        return false;
    }
    if matches!(std::env::var("TERM"), Ok(t) if t == "dumb") {
        return false;
    }
    // Piping output to a file or another program (`bruh query "..." > out.txt`, or
    // `| less`) shouldn't embed raw ANSI escape codes just because the user didn't also
    // remember to set NO_COLOR. std::io::IsTerminal has been stable since Rust 1.70, so
    // this needs no new dependency, just a check we weren't doing before.
    std::io::stdout().is_terminal()
}

fn c(code: &str, text: &str) -> String {
    if is_color_enabled() {
        format!("{}{}{}", code, text, RESET)
    } else {
        text.to_string()
    }
}

pub fn bold(s: &str) -> String {
    c(BOLD, s)
}
pub fn dim(s: &str) -> String {
    c(DIM, s)
}
pub fn green(s: &str) -> String {
    c(GREEN, s)
}
pub fn cyan(s: &str) -> String {
    c(CYAN, s)
}
// PALETTE-001: this is the one color for "pay attention", covers what red and yellow used
// to split between them (errors, warnings, disabled states, destructive confirmations, any
// non-zero exit code). One color for "something's off" is easier to keep consistent across
// a whole CLI than juggling where the red/yellow line falls in each file.
pub fn orange(s: &str) -> String {
    c(DEEP_ORANGE, s)
}

/// Convert a UTC timestamp to local time formatted as HH:MM:SS.
pub fn fmt_time(ts: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = ts.with_timezone(&Local);
    local.format("%H:%M:%S").to_string()
}

/// Convert a UTC timestamp to a human-readable local date/time.
pub fn fmt_datetime(ts: &DateTime<Utc>) -> String {
    let local: DateTime<Local> = ts.with_timezone(&Local);
    local.format("%a %b %d · %H:%M").to_string()
}

pub fn print_divider() {
    println!("{}", dim(&"─".repeat(56)));
}

pub fn print_header(title: &str) {
    print_divider();
    println!("  {}  ·  {}", bold(&cyan("bruh")), bold(title));
    print_divider();
}

pub fn print_footer() {
    print_divider();
}

/// Print an exit code badge: green [0] or deep orange [N].
pub fn exit_badge(code: i32) -> String {
    if code == 0 {
        green(&format!("[{}]", code))
    } else {
        orange(&format!("[{}]", code))
    }
}

/// Render a full Cognee recall() response as a human-readable timeline.
/// The response may be plain text, a JSON object with a "text" key, or arbitrary JSON.
pub fn print_timeline(response: &serde_json::Value, raw: bool) {
    if raw {
        if let Ok(s) = serde_json::to_string_pretty(response) {
            println!("{}", s);
        }
        return;
    }

    print_header("Memory Query");
    println!();

    // Try to extract meaningful text from the response.
    let text = extract_text(response);

    if text.trim().is_empty() {
        println!("  {}  No memory found for that query.", orange("○"));
        println!();
        println!(
            "  {}  Make sure the daemon is running: {}",
            dim("tip"),
            bold("bruh daemon &")
        );
    } else {
        // Render line by line, annotating patterns we recognise.
        for line in text.lines() {
            render_line(line);
        }
    }

    println!();
    print_footer();
}

fn render_line(line: &str) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        println!();
        return;
    }

    // Session header pattern: "Session: ..."
    if trimmed.starts_with("Session:") || trimmed.starts_with("session_") {
        println!("  {}", bold(&cyan(trimmed)));
        return;
    }

    // Error lines
    if trimmed.starts_with("error") || trimmed.starts_with("Error") || trimmed.starts_with("ERROR")
    {
        println!("  {}  {}", orange("✗"), orange(trimmed));
        return;
    }

    // Git commit lines. Cyan here rather than a warning color, a commit isn't bad news,
    // it's just a structural event worth calling out visually from the surrounding prose.
    if trimmed.starts_with("git commit") || trimmed.contains("commit -m") {
        println!("  {}  {}", cyan("◆"), bold(&cyan(trimmed)));
        return;
    }

    // Exit code patterns [0] / [1]
    if trimmed.contains("[0]") {
        println!(
            "  {}  {}",
            green("✓"),
            trimmed.replace("[0]", &green("[0]"))
        );
        return;
    }
    if trimmed.contains("[1]") || trimmed.contains("[2]") {
        let annotated = trimmed
            .replace("[1]", &orange("[1]"))
            .replace("[2]", &orange("[2]"));
        println!("  {}  {}", orange("✗"), annotated);
        return;
    }

    // Timestamp-prefixed lines: "14:32:18  command..."
    let parts: Vec<&str> = trimmed.splitn(2, "  ").collect();
    if parts.len() == 2 {
        let ts = parts[0].trim();
        // Simple heuristic: timestamp looks like HH:MM:SS
        if ts.len() == 8 && ts.chars().filter(|&c| c == ':').count() == 2 {
            println!("  {}  {}", dim(ts), bold(parts[1].trim()));
            return;
        }
    }

    // Default: Markdown-aware indent instead of dumping raw ** and - characters.
    println!("  {}", markdown_to_terminal(trimmed));
}

fn extract_text(v: &serde_json::Value) -> String {
    // Try common response fields in order of preference.
    for key in &["text", "result", "answer", "response", "content", "message"] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    // Try nested results array
    if let Some(arr) = v.get("results").and_then(|x| x.as_array()) {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| item.get("text").and_then(|t| t.as_str()))
            .map(|s| s.to_string())
            .collect();
        if !parts.is_empty() {
            return parts.join("\n");
        }
    }
    // Fall back to pretty JSON
    serde_json::to_string_pretty(v).unwrap_or_default()
}

/// Print a neat stats summary box.
pub fn print_stats_box(lines: &[(&str, String)]) {
    print_header("Developer Activity Report");
    println!();
    for (label, value) in lines {
        // Apply markdown rendering to the value
        let rendered_value = markdown_to_terminal(&value);
        println!(
            "  {}  {}",
            cyan(&format!("{:<28}", label)),
            bold(&rendered_value)
        );
    }
    println!();
    print_footer();
}

/// COGNEE-018: recall() responses are LLM-generated prose meant for a chat UI,
/// headings and emphasis come back as literal Markdown ("**Developer Hand-off
/// Brief**", "- Last-session activity"), which just showed up as raw asterisks and
/// dashes on screen. This renders the common cases (bold, bullets) into something
/// that actually reads cleanly on a terminal instead of dumping the Markdown as-is.
fn markdown_to_terminal(line: &str) -> String {
    let trimmed = line.trim_start();
    let (prefix, rest) = if let Some(stripped) = trimmed
        .strip_prefix("- ")
        .or_else(|| trimmed.strip_prefix("* "))
    {
        (cyan("• "), stripped)
    } else {
        (String::new(), trimmed)
    };

    let mut out = String::with_capacity(rest.len());
    let mut chars = rest.chars().peekable();
    let mut bold_open = false;
    while let Some(ch) = chars.next() {
        if ch == '*' && chars.peek() == Some(&'*') {
            chars.next(); // consume the second '*'
            if is_color_enabled() {
                out.push_str(if bold_open { RESET } else { BOLD });
            }
            bold_open = !bold_open;
        } else {
            out.push(ch);
        }
    }
    if bold_open && is_color_enabled() {
        out.push_str(RESET); // unterminated **, don't bleed bold into later output
    }
    format!("{}{}", prefix, out)
}

/// Print the bruh explain brief.
pub fn print_explain(directory: &str, narrative: &str) {
    print_header(&format!("Context Brief  ·  {}", directory));
    println!();
    for line in narrative.lines() {
        if line.trim().is_empty() {
            println!();
        } else {
            println!("  {}", markdown_to_terminal(line));
        }
    }
    println!();
    print_footer();
}

/// Inline bruh watch annotation.
pub fn print_watch_memory(header: &str, body: &str) {
    let bar = if is_color_enabled() {
        format!("{}{}{}", CYAN, "── bruh memory ", RESET)
    } else {
        "── bruh memory ".to_string()
    };
    let fill = "─".repeat(56usize.saturating_sub(15));
    println!("{}{}", bar, dim(&fill));
    println!("  {}", bold(header));
    for line in body.lines() {
        println!("  {}", line);
    }
    let close = if is_color_enabled() {
        format!("{}{}{}", CYAN, "─".repeat(56), RESET)
    } else {
        "─".repeat(56)
    };
    println!("{}", close);
}

