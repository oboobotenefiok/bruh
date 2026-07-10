// bruh config list / get / set, no hand-editing required by the user here.
// This is the thin CLI-facing wrapper around the actual Config logic living in cli/config.rs.
// Config itself doesn't know or care about terminal formatting, colors, or how the CLI
// arguments got parsed, this file's whole job is bridging "what the user typed" to "what
// Config's methods expect" and then printing something nice back.

// We bring in the custom pretty printer
use crate::cli::{
    output::{bold, cyan, dim, green, print_footer, print_header},
    Config,
};
use anyhow::Result;

// Deliberately hand-maintained rather than reflecting over Config's fields at runtime
// (Rust doesn't give us that without extra crates for a project this size). The
// config_keys_cover_every_config_field test below is what actually keeps this honest: it
// serializes a real Config to JSON and asserts every field name shows up here too, so
// forgetting to add a new field here fails `cargo test` instead of just silently never
// appearing in `bruh config list`.
const DISPLAY_KEYS: &[&str] = &[
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
    "offline_buffer_path",
    "excluded_commands",
];

// sub is the subcommand ("list", "get", or "set"), already extracted from argv in main.rs.
// key and value are Options since "list" needs neither, "get" needs just key, and "set"
// needs both.
pub fn run(sub: &str, key: Option<&str>, value: Option<&str>) -> Result<()> {
    match sub {
        "list" => {
            let cfg = Config::load()?;
            print_header("Configuration");
            println!();
            for k in DISPLAY_KEYS {
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

#[cfg(test)]
mod tests {
    use super::*;

    // Serializes a real Config to JSON and checks its field names against DISPLAY_KEYS in
    // both directions: every JSON field should be listed here, and everything listed here
    // should be a real field. Whichever direction breaks tells you exactly what drifted,
    // add a field to Config and forget to list it here, or list a key that no longer (or
    // never did) exist on the struct.
    #[test]
    fn config_keys_cover_every_config_field() {
        let cfg = Config::default();
        let json = serde_json::to_value(&cfg).expect("Config should serialize");
        let fields = json.as_object().expect("Config serializes as an object");

        for field_name in fields.keys() {
            assert!(
                DISPLAY_KEYS.contains(&field_name.as_str()),
                "Config field '{}' exists on the struct but isn't in DISPLAY_KEYS, \
                 it'll never show up in `bruh config list`",
                field_name
            );
        }

        for key in DISPLAY_KEYS {
            assert!(
                fields.contains_key(*key),
                "DISPLAY_KEYS lists '{}' but Config has no such field anymore",
                key
            );
        }
    }

    #[test]
    fn every_display_key_resolves_through_get_value() {
        // The other half of the guarantee: being listed in DISPLAY_KEYS is only useful if
        // get_value() actually knows how to render it. This would have caught
        // offline_buffer_path and excluded_commands sitting on the struct with no display
        // support at all before this fix.
        let cfg = Config::default();
        for key in DISPLAY_KEYS {
            assert!(
                cfg.get_value(key).is_some(),
                "'{}' is in DISPLAY_KEYS but get_value() doesn't know how to render it",
                key
            );
        }
    }
}
