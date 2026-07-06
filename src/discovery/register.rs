use crate::events::PackageManagerProfile;
use anyhow::Result;
use serde_json::json;
// This file helps to send package manager profiles to cognee,
pub async fn store_profile(profile: &PackageManagerProfile) -> Result<()> {
    // If cognee is not cinfigured, we'll just fail silently. That's the best thing we can do for now.

    let client = match crate::cognee::CogneeClient::from_config() {
        Ok(c) => c, // We extract for client if available
        Err(_) => return Ok(()), // Otherwise we return the wrapped unit type to satisfy the contract.
    };

// We structure it as a json to post.
    let body = json!({
        "documents": [format!(
            "PACKAGE_MANAGER_PROFILE: {} install:{} list:{} confidence:{}",
            profile.name, profile.install_verb, profile.list_command, profile.confidence
        )]
    });
// And send to storage
    client.post("remember", body).await?;
    Ok(())
}
