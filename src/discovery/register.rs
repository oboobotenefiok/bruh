use crate::events::PackageManagerProfile;
use anyhow::Result;
use serde_json::json;

// This file helps send package manager profiles to Cognee.

/// What actually happened when we tried to store a profile. This exists so callers can tell
/// "we genuinely stored this in Cognee" apart from "we skipped storing it because Cognee
/// isn't configured yet." Those two outcomes used to both come back as a plain Ok(()), which
/// meant `bruh managers --learn` could print a green checkmark for a store that never
/// actually happened, since nothing distinguished the two cases at the type level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreOutcome {
    /// The profile made it into Cognee.
    Stored,
    /// Cognee isn't configured yet, so we skipped the remote store on purpose. Local caching
    /// (see discovery::cache) still happens regardless, this only covers the Cognee side.
    NotConfigured,
}

pub async fn store_profile(profile: &PackageManagerProfile) -> Result<StoreOutcome> {
    // If Cognee isn't configured, we don't treat that as an error, someone can perfectly
    // well use bruh's local package-manager cache before they've ever set up a Cognee key.
    // But we do need the caller to know this happened, rather than quietly reporting success
    // for a network call that was never made.
    let client = match crate::cognee::CogneeClient::from_config() {
        Ok(c) => c,
        Err(_) => return Ok(StoreOutcome::NotConfigured),
    };

    // We structure it as a JSON document to post.
    let body = json!({
        "documents": [format!(
            "PACKAGE_MANAGER_PROFILE: {} install:{} list:{} confidence:{}",
            profile.name, profile.install_verb, profile.list_command, profile.confidence
        )]
    });

    // And send it off to storage. A real failure here (bad key, Cognee down) still
    // propagates as an Err, same as before, only the "not configured at all" case gets
    // special treatment.
    client.post("remember", body).await?;
    Ok(StoreOutcome::Stored)
}
