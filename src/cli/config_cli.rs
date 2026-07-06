// bruh config list / get / set, no hand-editing required by the user here.
// This is the thin CLI-facing wrapper around the actual Config logic living in cli/config.rs.
// Config itself doesn't know or care about terminal formatting, colors, or how the CLI
// arguments got parsed, this file's whole job is bridging "what the user typed" to "what
// Config's methods expect" and then printing something nice back.

// We bring in the custom pretty printer
use crate::cli::output::{bold, cyan, dim, green, print_footer, print_header};
use crate::cli::Config;
use anyhow::Result;

// sub is the subcommand ("list", "get", or "set"), already extracted from argv in main.rs.
// key and value are Options since "list" needs neither, "get" needs just key, and "set"
// needs both.
pub fn run(sub: &str, key: Option<&str>, value: Option<&str>) -> Result<()> {
    match sub {
        "list" => {
            let cfg = Config::load()?;
            print_header("Configuration");
            println!();
            // Deliberately hand-maintained list of keys here rather than trying to
            // reflect over the Config struct's fields (Rust doesn't give us easy runtime
            // struct field iteration without extra crates), so if a new field gets added
            // to Config, remember to add it here too or it just won't show up in the list.
            let keys = [
                "cognee_api_key",
                "cognee_api_url",
                "gemini_api_key",
                "groq_api_key",
                "claude_api_key",
                "llm_priority",
                "discovery_enabled",
                "discovery_rate_limit_seconds",
                "poll_interval_seconds",
                "batch_flush_interval_seconds",
                "max_buffer_size",
                "daemon_log_level",
            ];
            for k in &keys {
                // Get the configs and assign to each the indexes in the array of keys
                if let Some(v) = cfg.get_value(k) {
                    println!("  {}  {}", cyan(&format!("{:<35}", k)), bold(&v));
                }
            }
            println!();
            println!(
                "  Config file: {}",
                dim(&Config::config_path()?.to_string_lossy())
            );
            println!();
            print_footer();
        }
        "get" => {
            let k = key.ok_or_else(|| anyhow::anyhow!("Usage: bruh config get <key>"))?;
            let cfg = Config::load()?;
            match cfg.get_value(k) {
                Some(v) => println!("{}", v),
                None => anyhow::bail!("Unknown key '{}'. Run 'bruh config list'.", k),
            }
        }
        "set" => {
            let k = key.ok_or_else(|| anyhow::anyhow!("Usage: bruh config set <key> <value>"))?;
            let v = value.ok_or_else(|| anyhow::anyhow!("Usage: bruh config set <key> <value>"))?;
            let mut cfg = Config::load()?;
            // set_value does the actual parsing and validation (see cli/config.rs), we
            // just propagate any error with ? and only save to disk if it succeeded, so a
            // bad value never gets persisted over a working config.
            cfg.set_value(k, v)?;
            cfg.save()?;
            println!("  {} {} = {}", green("✓"), bold(k), v);
        }
        _ => {
            anyhow::bail!("Usage: bruh config <list|get|set> [key] [value]");
        }
    }
    Ok(()) // Satisfy the contract.
}
