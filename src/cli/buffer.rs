//! FIX-BUFFER-012: New `bruh buffer` command for debugging and managing the offline buffer.
//!
//! This command provides visibility into the buffer state and allows operators to
//! diagnose and resolve buffer issues without manually inspecting raw NDJSON files.
//!
//! Commands:
//!   bruh buffer status   - Show buffer size, corrupt lines, backoff state
//!   bruh buffer clear    - Force clear the buffer (with confirmation)
//!   bruh buffer retry    - Force a retry of the buffer flush
//!   bruh buffer inspect  - Show the first/last few events in the buffer

use crate::{
    cli::{
        output::{bold, cyan, dim, green, orange, print_footer, print_header},
        Config,
    },
    daemon::buffer::{self},
    events::Event,
};
use anyhow::{Context, Result};
use std::io::{self, Write};

/// Run the buffer subcommand based on the provided arguments.
pub async fn run(subcommand: Option<&str>, args: &[String]) -> Result<()> {
    let sub = subcommand.unwrap_or("status");

    match sub {
        "status" => buffer_status().await,
        "clear" => buffer_clear(args).await,
        "retry" => buffer_retry().await,
        "inspect" => buffer_inspect(args).await,
        _ => {
            anyhow::bail!("Unknown buffer subcommand: {}\n\nUsage:\n  bruh buffer status\n  bruh buffer clear\n  bruh buffer retry\n  bruh buffer inspect", sub);
        }
    }
}

/// Show the current state of the offline buffer.
/// This reads the buffer file and displays its size, corruption count, and backoff status.
async fn buffer_status() -> Result<()> {
    print_header("Buffer Status");
    println!();

    let config = match Config::load() {
        Ok(c) => c,
        Err(e) => {
            println!("  {} Failed to load config: {}", orange("✗"), e);
            println!();
            print_footer();
            return Ok(());
        }
    };

    let buf_path = config.offline_buffer_path;
    let backoff_remaining = buffer::get_backoff_seconds();
    let should_retry = buffer::should_retry();

    // Read the buffer file
    let content = match tokio::fs::read_to_string(&buf_path).await {
        Ok(c) => c,
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                println!("  {} Buffer file does not exist.", dim("→"));
                println!("  {} No buffered events.", green("✓"));
                println!();
                print_footer();
                return Ok(());
            }
            println!("  {} Failed to read buffer: {}", orange("✗"), e);
            println!();
            print_footer();
            return Ok(());
        }
    };

    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total_lines = lines.len();

    // Count corrupt lines
    let mut corrupt_count = 0;
    for line in &lines {
        if serde_json::from_str::<Event>(line).is_err() {
            corrupt_count += 1;
        }
    }

    let valid_count = total_lines - corrupt_count;

    println!("  {} Buffer path:", dim("│"));
    println!("  {}   {}", dim("│"), bold(&buf_path.to_string_lossy()));
    println!("  {} Total lines:    {}", dim("│"), bold(&total_lines.to_string()));
    println!("  {} Valid events:   {}", dim("│"), green(&valid_count.to_string()));
    println!("  {} Corrupt lines:  {}", dim("│"), if corrupt_count > 0 {
        orange(&corrupt_count.to_string())
    } else {
        green("0")
    });

    println!("  {} Backoff status:", dim("│"));
    if backoff_remaining > 0 {
        println!("  {}   {} seconds remaining", dim("│"), orange(&backoff_remaining.to_string()));
        println!("  {}   Flushes are paused. Try: {}", dim("│"), bold("bruh buffer retry"));
    } else if should_retry {
        println!("  {}   {}", dim("│"), green("Ready to retry"));
    } else {
        println!("  {}   {}", dim("│"), dim("Backoff cleared, waiting for flush timer"));
    }

    let chunk_size = 500;
    let chunks = (total_lines + chunk_size - 1) / chunk_size;
    println!("  {} Estimated chunks: {}", dim("│"), chunks);

    if total_lines > 0 {
        let bytes = content.len();
        let kb = bytes / 1024;
        println!("  {} File size: {} KB", dim("│"), kb);
    }

    println!();
    print_footer();
    Ok(())
}

/// Force clear the offline buffer with confirmation.
/// This is destructive and will lose all buffered events.
async fn buffer_clear(args: &[String]) -> Result<()> {
    let force = args.iter().any(|a| a == "--force");

    print_header("Clear Buffer");
    println!();

    let config = Config::load()?;
    let buf_path = config.offline_buffer_path;

    // Check if the buffer exists
    if !tokio::fs::try_exists(&buf_path).await? {
        println!("  {} Buffer file does not exist.", dim("→"));
        println!();
        print_footer();
        return Ok(());
    }

    // Read the current size
    let content = tokio::fs::read_to_string(&buf_path).await?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        println!("  {} Buffer is already empty.", dim("→"));
        println!();
        print_footer();
        return Ok(());
    }

    println!("  {} This will clear {} buffered events.", orange("!"), bold(&total_lines.to_string()));
    println!("  {} This action is irreversible.", orange("!"));

    if !force {
        println!();
        print!("  Continue? [y/N]: ");
        io::stdout().flush()?;
        let mut ans = String::new();
        io::stdin().read_line(&mut ans)?;
        if !matches!(ans.trim().to_lowercase().as_str(), "y" | "yes") {
            println!("  Cancelled.");
            println!();
            print_footer();
            return Ok(());
        }
    }

    // Clear the buffer
    tokio::fs::write(&buf_path, "").await
        .with_context(|| format!("Failed to clear buffer at {:?}", buf_path))?;

    println!("  {} Buffer cleared ({} events removed).", green("✓"), total_lines);

    // Also reset the retry state to allow flushes
    buffer::record_success();
    println!("  {} Backoff state reset.", green("✓"));

    println!();
    print_footer();
    Ok(())
}

/// Force a retry of the buffer flush by resetting the backoff state.
async fn buffer_retry() -> Result<()> {
    print_header("Buffer Retry");
    println!();

    let config = Config::load()?;
    let buf_path = config.offline_buffer_path;

    // Check if the buffer exists
    if !tokio::fs::try_exists(&buf_path).await? {
        println!("  {} Buffer file does not exist.", dim("→"));
        println!();
        print_footer();
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&buf_path).await?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
    let total_lines = lines.len();

    if total_lines == 0 {
        println!("  {} Buffer is already empty.", dim("→"));
        println!();
        print_footer();
        return Ok(());
    }

    // Reset the backoff state
    buffer::record_success();
    println!("  {} Backoff state reset.", green("✓"));

    println!("  {} {} events are ready to be flushed.", dim("→"), bold(&total_lines.to_string()));
    println!("  {} The next flush tick will attempt to send them to Cognee.", dim("→"));

    // Force a flush signal to make the daemon attempt immediately
    if let Ok(data_dir) = Config::data_dir() {
        let signal_path = data_dir.join("flush_now");
        let timestamp = chrono::Utc::now().to_rfc3339();
        if let Err(e) = tokio::fs::write(&signal_path, timestamp).await {
            println!("  {} Could not send flush signal: {}", orange("○"), e);
            println!("  {} Wait for the next flush tick or run: {}", dim("→"), bold("bruh daemon --flush-now"));
        } else {
            println!("  {} Flush signal sent to daemon.", green("✓"));
        }
    }

    println!();
    print_footer();
    Ok(())
}

/// Inspect the buffer by showing the first/last few events.
async fn buffer_inspect(args: &[String]) -> Result<()> {
    let mut count = 10;
    let mut show_last = false;

    for arg in args {
        if arg == "--last" {
            show_last = true;
        } else if arg.starts_with("--count=") {
            if let Ok(n) = arg.trim_start_matches("--count=").parse::<usize>() {
                count = n;
            }
        }
    }

    print_header("Buffer Inspect");
    println!();

    let config = Config::load()?;
    let buf_path = config.offline_buffer_path;

    if !tokio::fs::try_exists(&buf_path).await? {
        println!("  {} Buffer file does not exist.", dim("→"));
        println!();
        print_footer();
        return Ok(());
    }

    let content = tokio::fs::read_to_string(&buf_path).await?;
    let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();

    if lines.is_empty() {
        println!("  {} Buffer is empty.", dim("→"));
        println!();
        print_footer();
        return Ok(());
    }

    let total_lines = lines.len();
    let display_lines: Vec<&str> = if show_last {
        let start = total_lines.saturating_sub(count);
        lines[start..].to_vec()
    } else {
        lines.iter().take(count).copied().collect()
    };

    let actually_displayed = display_lines.len();

    if show_last {
        println!("  {} Showing last {} lines of {} total:", dim("→"), actually_displayed, total_lines);
    } else {
        println!("  {} Showing first {} lines of {} total:", dim("→"), actually_displayed, total_lines);
    }
    println!();

    let chunk_size = 500;
    let mut line_number = if show_last {
        total_lines.saturating_sub(count)
    } else {
        0
    };

    for line in display_lines {
        let chunk = line_number / chunk_size;
        println!("  {}  [chunk {}, line {}]  {}", dim("│"), chunk, line_number, dim("─"));

        // Pretty print the event if it's valid JSON
        if let Ok(event) = serde_json::from_str::<Event>(line) {
            let summary = match &event {
                Event::ShellCommand(e) => format!("shell: {}", e.command),
                Event::PackageInstall(e) => format!("package: {} {}", e.manager, e.package),
                Event::GitCommit(e) => format!("git: {} ({})", e.message, e.hash),
                Event::PackageManagerProfile(p) => format!("profile: {}", p.name),
            };
            println!("  {}  {}", dim("│"), cyan(&summary));
        } else {
            println!("  {}  {}", dim("│"), orange("CORRUPT LINE"));
            // Show a preview of the corrupt line
            let preview = if line.len() > 80 {
                format!("{}...", &line[..80])
            } else {
                line.to_string()
            };
            println!("  {}  {}", dim("│"), dim(&preview));
        }

        line_number += 1;
    }

    println!("  {}", dim("│"));
    println!();
    print_footer();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_buffer_status_defaults() {
        // Just verify the function signatures and that the module compiles
        // Full tests would require mocking the filesystem
    }

    #[test]
    fn test_chunk_size_alignment() {
        let chunk_size = 500;
        // 1000 events = 2 chunks
        assert_eq!((1000 + chunk_size - 1) / chunk_size, 2);
        // 1 event = 1 chunk
        assert_eq!((1 + chunk_size - 1) / chunk_size, 1);
        // 0 events = 0 chunks
        assert_eq!((0 + chunk_size - 1) / chunk_size, 0);
    }
}
