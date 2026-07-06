//! COGNEE-004: improve() returns a summary string for the CLI to display.
//!
//! COGNEE-021: this used to post to "/api/v1/improve", which doesn't exist on Cognee
//! Cloud at all. A live test against a real tenant confirmed a plain 404 on that path.
//! Cross-checking Cognee's own docs (the http-server router list, and the cognee-n8n
//! integration, which targets this exact kind of tenant) turned up the real verb:
//! Cognee Cloud's graph-build step is called "cognify", not "improve". "Improve" only
//! ever existed as a name inside the Python SDK's convenience wrapper around add plus
//! cognify, it was never an actual route on the wire. We're keeping bruh's own function
//! and CLI command named `improve` on purpose, since that's still a perfectly good name
//! for what this does from a user's point of view, we're just pointing it at the real
//! endpoint underneath.

use super::CogneeClient;
use anyhow::Result;
use serde_json::json;

/// COGNEE-020: added the `background` parameter. Before this, improve() always sent
/// `run_in_background: false`, meaning it always blocked until the whole graph enrichment
/// pass finished. That's exactly what you want when a person types `bruh improve` and is
/// sitting there watching the terminal, they want a real "done, here's your summary"
/// moment, not a fire-and-forget. But it's the wrong choice for the daemon's own periodic
/// trigger (see daemon/mod.rs), which needs to kick off enrichment on a timer without
/// blocking its main loop while an LLM chews through the graph.
///
/// COGNEE-021: the `background` flag no longer changes anything we send to Cognee itself.
/// The real cognify endpoint's body shape, confirmed against the docs, is just
/// `{"datasets": [...]}`, there's no documented flag for asking the server to run it
/// asynchronously. That's fine though, because the actual "don't block" behavior was
/// always coming from bruh's own side anyway: daemon/mod.rs already wraps this whole call
/// in `tokio::spawn(...)` before awaiting it, so the daemon's main loop never waits on
/// this function regardless of what we send. We keep the `background` parameter here so
/// neither call site (cli/improve.rs or daemon/mod.rs) needs to change, it's just now a
/// note to a future reader about which caller this is, rather than something we forward
/// over the wire.
pub async fn improve(background: bool) -> Result<Option<String>> {
    // COGNEE-013: shared client, see cognee/mod.rs.
    let client = CogneeClient::shared()?;

    // COGNEE-021: the real endpoint is "cognify", and it wants an array of dataset
    // names, not a single "dataset_name" string. We still enrich the same dataset bruh
    // ingests activity into, using the shared DATASET_NAME constant, so this can never
    // drift from what ingest.rs writes to or query.rs reads from.
    let body = json!({
        "datasets": [super::DATASET_NAME],
    });

    log::info!(
        "Triggering cognify for dataset '{}' (background caller: {})…",
        super::DATASET_NAME,
        background
    );

    // COGNEE-021: 10 minutes, matching what Cognee's own docs say cognify can need on a
    // larger dataset. The shared client's normal 120s default is fine for add/recall/
    // forget, but would cut a genuinely slow cognify pass off mid-flight and make it
    // look like a failure when it might have just needed more time.
    let resp = client
        .post_with_timeout("cognify", body, Some(std::time::Duration::from_secs(600)))
        .await?;

    // Extract any summary text Cognee returns. Different response shapes have shown up
    // in practice, so we check a few plausible keys rather than betting on just one.
    let text = resp
        .get("text")
        .or_else(|| resp.get("result"))
        .or_else(|| resp.get("summary"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    Ok(text)
}
