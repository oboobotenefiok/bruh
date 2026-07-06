//! CORE-003: bruh daemon --status, reads the health file written by the daemon.
// This is deliberately a read-only, file-based status check rather than actually talking
// to the running daemon process (no IPC, no socket round trip). The daemon writes a fresh
// health.json every flush tick (see write_health() in daemon/mod.rs), so this just reads
// that snapshot and pretty-prints it. Simple, and it means `bruh daemon --status` works
// even if something's gone wrong enough that the daemon can't respond to a live query,
// as long as the file's still there from its last successful tick.

use crate::cli::output::{bold, dim, green, orange, print_footer, print_header};
use crate::cli::Config;
use anyhow::Result;

pub fn run() -> Result<()> {
    let health_path = Config::health_file_path()?;

    print_header("Daemon Status");
    println!();

    // No health file at all means either the daemon has never run, or it was killed hard
    // enough that cleanup_sockets() in daemon/mod.rs never got a chance to remove the old
    // file, either way, "not running" is the honest answer to give here since we have no
    // fresher information to go on.
    if !health_path.exists() {
        println!("  {} Daemon is not running.", orange("●"));
        println!();
        println!("  Start with: {}", bold("bruh daemon &"));
        println!();
        print_footer();
        return Ok(());
    }

    let content = std::fs::read_to_string(&health_path).unwrap_or_else(|_| "{}".into());
    let v: serde_json::Value = serde_json::from_str(&content).unwrap_or_default();

    // Every field below is read defensively with .as_str()/.as_u64() plus an if-let, so a
    // partially written or slightly-out-of-date health.json (say, from an older bruh
    // version with fewer fields) just shows fewer status lines instead of erroring out.
    let status = v["status"].as_str().unwrap_or("unknown");
    let status_icon = match status {
        "running" => green("●"),
        _ => orange("●"),
    };

    println!("  {}  Status:            {}", status_icon, bold(status));

    if let Some(uptime) = v["uptime_seconds"].as_u64() {
        let h = uptime / 3600;
        let m = (uptime % 3600) / 60;
        let s = uptime % 60;
        println!("  {}  Uptime:            {}h {}m {}s", dim("│"), h, m, s);
    }

    if let Some(n) = v["events_queued"].as_u64() {
        println!("  {}  Events queued:     {}", dim("│"), n);
    }

    if let Some(n) = v["buffered_events"].as_u64() {
        let label = if n > 0 {
            orange(&n.to_string())
        } else {
            n.to_string()
        };
        println!("  {}  Buffered (offline):{}", dim("│"), label);
    }

    if let Some(ts) = v["last_flush_time"].as_str() {
        println!("  {}  Last flush:        {}", dim("│"), ts);
    }

    if let Some(st) = v["last_flush_status"].as_str() {
        let formatted = if st == "success" { green(st) } else { orange(st) };
        println!("  {}  Flush status:      {}", dim("│"), formatted);
    }

    if let Some(n) = v["managers_known"].as_u64() {
        println!("  {}  Managers known:    {}", dim("│"), n);
    }

    if let Some(n) = v["managers_learned"].as_u64() {
        println!("  {}  Managers learned:  {}", dim("│"), n);
    }

    println!();
    print_footer();
    Ok(())
}
