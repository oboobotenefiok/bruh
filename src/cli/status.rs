//! CORE-003: bruh daemon --status, reads the health file written by the daemon.
//! CLI-NEW-001: bruh daemon --flush-now, forces a flush and resets backoff.
// This is deliberately a read-only, file-based status check rather than actually talking
// to the running daemon process (no IPC, no socket round trip). The daemon writes a fresh
// health.json every flush tick (see write_health() in daemon/mod.rs), so this just reads
// that snapshot and pretty-prints it. Simple, and it means `bruh daemon --status` works
// even if something's gone wrong enough that the daemon can't respond to a live query,
// as long as the file's still there from its last successful tick.

use crate::cli::{
    output::{bold, dim, fmt_datetime, fmt_time, green, orange, print_footer, print_header},
    Config,
};
use anyhow::Result;
use chrono::{DateTime, Utc};

// A daemon in good health rewrites health.json every flush tick, so a snapshot older than
// a few flush intervals almost certainly means the process died without going through
// cleanup_sockets() (a hard kill, an OOM kill, a crash), not that it's just running quietly.
// Multiplying by 3 gives it enough slack to ride out one or two slow/failed flush cycles
// without crying wolf on a daemon that's actually fine.
const STALE_MULTIPLIER: u64 = 3;

/// CLI-NEW-001: force a flush by sending a signal file to the daemon.
/// This resets the backoff state and tells the daemon to attempt a flush on its next tick.
pub fn force_flush() -> Result<()> {
    let data_dir = Config::data_dir()?;
    let signal_path = data_dir.join("flush_now");
    
    // Create the directory if it doesn't exist
    if let Some(parent) = signal_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    
    // Write the signal file with the current timestamp
    let timestamp = Utc::now().to_rfc3339();
    std::fs::write(&signal_path, timestamp)?;
    
    println!();
    println!("  {} Force flush signal sent to daemon.", green("✓"));
    println!("  {} Check status with: {}", dim("→"), bold("bruh daemon --status"));
    println!();
    
    Ok(())
}

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

    // A daemon that died without a clean shutdown leaves its last real health.json behind
    // forever, since nothing else ever deletes it. Comparing "as_of" (written fresh on every
    // flush tick) against right now is how we tell that stale snapshot apart from a genuinely
    // live daemon, rather than trusting the file's mere existence at face value.
    let flush_interval = Config::load()
        .map(|c| c.batch_flush_interval_seconds)
        .unwrap_or(240);
    let stale_after = flush_interval.saturating_mul(STALE_MULTIPLIER);
    let is_stale = v["as_of"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|as_of| {
            let age = Utc::now().signed_duration_since(as_of.with_timezone(&Utc));
            age.num_seconds() > stale_after as i64
        })
        // No "as_of" at all (an older health.json written before this field existed) can't
        // be judged for freshness one way or the other, so we don't flag it as stale.
        .unwrap_or(false);

    let status_icon = match (status, is_stale) {
        ("running", false) => green("●"),
        _ => orange("●"),
    };

    let status_label = if is_stale {
        format!("{} (stale)", status)
    } else {
        status.to_string()
    };
    println!(
        "  {}  Status:            {}",
        status_icon,
        bold(&status_label)
    );

    if is_stale {
        println!(
            "  {}  {}",
            dim("│"),
            orange("Last update looks old, the daemon may have stopped responding.")
        );
        println!(
            "  {}  {}",
            dim("│"),
            orange("Try: bruh daemon --flush-now  or  restart the daemon")
        );
    }

    if let Some(checked_at) = v["as_of"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        println!(
            "  {}  Checked at:        {}",
            dim("│"),
            fmt_time(&checked_at.with_timezone(&Utc))
        );
    }

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
        
        // Show recommendation if buffer is getting large
        if n > 100 {
            println!(
                "  {}  {}",
                dim("│"),
                orange("Large buffer detected. Try: bruh daemon --flush-now")
            );
        }
    }

    if let Some(ts) = v["last_flush_time"]
        .as_str()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
    {
        println!(
            "  {}  Last flush:        {}",
            dim("│"),
            fmt_datetime(&ts.with_timezone(&Utc))
        );
    }

    if let Some(st) = v["last_flush_status"].as_str() {
        let formatted = if st == "success" {
            green(st)
        } else {
            orange(st)
        };
        println!("  {}  Flush status:      {}", dim("│"), formatted);
        
        // Show recommendation for failed flushes
        if st == "failed" {
            println!(
                "  {}  {}",
                dim("│"),
                orange("Check your Cognee API key with: bruh config get cognee_api_key")
            );
            println!(
                "  {}  {}",
                dim("│"),
                orange("Or try: bruh daemon --flush-now")
            );
        }
    }

    if let Some(backoff_secs) = v["backoff_seconds"].as_u64() {
        if backoff_secs > 0 {
            println!(
                "  {}  Backoff:           {} seconds remaining",
                dim("│"),
                backoff_secs
            );
            println!(
                "  {}  {}",
                dim("│"),
                orange("Flushes are paused. Try: bruh daemon --flush-now")
            );
        }
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

