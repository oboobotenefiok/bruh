// src/discovery/register.rs
use crate::events::PackageManagerProfile;
use anyhow::Result;
// serde_json::json no longer needed since we're using multipart form

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

    // We structure it as a multipart form. Just like add/, the /remember endpoint expects multipart/form-data,
    // not JSON. The datasetName field tells Cognee which dataset to store this in, using the
    // same DATASET_NAME constant that every other Cognee operation (ingest, improve, query,
    // forget) already uses to keep everything consistent.
    let documents_text = format!(
        "PACKAGE_MANAGER_PROFILE: {} install:{} list:{} confidence:{}",
        profile.name, profile.install_verb, profile.list_command, profile.confidence
    );

    // Use the existing post_multipart method on CogneeClient. It takes a closure that
    // builds the form, which is retried on each attempt if the request fails.
    client
        .post_multipart("remember", || {
            reqwest::multipart::Form::new()
                .text("datasetName", crate::cognee::DATASET_NAME.to_string())
                .text("documents", documents_text.clone())
        })
        .await?;

    Ok(StoreOutcome::Stored)
}
