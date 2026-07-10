//! DISCOVERY-007: 30-day TTL on learned managers.
// Whenever discovery successfully figures out a new package manager, we don't want to pay
// the cost (web search plus an LLM call) every single time we see it again. So we cache the
// result to disk. But package managers evolve (install syntax changes, registries move) so
// I don't want to trust a cached answer forever either, hence the 30 day expiry below.

use crate::{cli::Config, events::PackageManagerProfile};
use anyhow::Result;
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

const CACHE_TTL_DAYS: i64 = 30;

// Wrapping the profile in its own struct instead of storing PackageManagerProfile directly
// felt like it'd give me room to add cache-specific metadata later (hit counts, last-used
// timestamp, that kind of thing) without having to touch the core event schema. Hasn't
// needed it yet, but the wrapper costs nothing to keep.
#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    profile: PackageManagerProfile,
}

// Reads the whole learned-managers file and returns only the entries that haven't expired.
// If the file doesn't exist yet (first run, nothing learned yet) we just hand back an empty
// map instead of erroring, since "nothing learned" is a perfectly normal state, not a
// failure.
pub fn load_learned_managers() -> Result<HashMap<String, PackageManagerProfile>> {
    let path = Config::learned_managers_path()?;
    if !path.exists() {
        return Ok(HashMap::new());
    }

    let content = std::fs::read_to_string(&path)?;
    // unwrap_or_default() here means a corrupted or empty cache file quietly becomes an
    // empty map rather than crashing the whole daemon over a JSON parse error. Losing the
    // cache is annoying (we'll just re-discover things) but it's recoverable, so it's not
    // worth treating as a hard failure.
    let raw: HashMap<String, CacheEntry> = serde_json::from_str(&content).unwrap_or_default();

    let cutoff = Utc::now() - Duration::days(CACHE_TTL_DAYS);

    // DISCOVERY-007: filter out expired entries
    let valid: HashMap<String, PackageManagerProfile> = raw
        .into_iter()
        .filter(|(_, entry)| entry.profile.discovered_at > cutoff)
        .map(|(k, entry)| (k, entry.profile))
        .collect();

    Ok(valid)
}

// Adds (or overwrites) one profile in the cache file. We read the existing map first,
// insert/replace the one entry, then write the whole thing back out. Not the most efficient
// approach for a huge cache, but realistically nobody's going to have thousands of package
// managers cached, so a full read-modify-write is simple and fine here.
pub fn save_learned_manager(profile: &PackageManagerProfile) -> Result<()> {
    let path = Config::learned_managers_path()?;
    if let Some(p) = path.parent() {
        std::fs::create_dir_all(p)?;
    }

    let mut existing: HashMap<String, CacheEntry> = if path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&path)?).unwrap_or_default()
    } else {
        HashMap::new()
    };

    existing.insert(
        profile.name.clone(),
        CacheEntry {
            profile: profile.clone(),
        },
    );
    std::fs::write(&path, serde_json::to_string_pretty(&existing)?)?;
    Ok(())
}
