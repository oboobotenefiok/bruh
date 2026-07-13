//! COGNEE-003: recall() with multi-format response parsing + fallback.

use super::CogneeClient;
use anyhow::Result;
use serde_json::{json, Value};

/// Asks Cognee a natural-language question over everything ingested into [`DATASET_NAME`](super::DATASET_NAME),
/// returning the parsed JSON response.
///
/// # Errors
///
/// Returns an error if the request to Cognee fails.
pub async fn recall(query: &str) -> Result<Value> {
    let client = CogneeClient::shared()?;
    // COGNEE-016: /api/v1/recall (confirmed against docs.cognee.ai/api-reference/recall/recall)
    // takes a *camelCase* body: searchType, datasets, datasetIds, systemPrompt, nodeName,
    // topK, onlyContext, verbose, unlike /api/v1/search's snake_case. We were sending
    // "search_type" (snake_case), which Cognee's Pydantic model just silently drops as an
    // unrecognised field and falls back to its own default (GRAPH_COMPLETION, so functionally
    // harmless by coincidence), but the real cost was that we sent no "datasets" field at all,
    // so every query searched whatever the tenant's default dataset is instead of
    // wherever remember() actually writes (see ingest.rs). Scoping to DATASET_NAME here,
    // the same constant ingest and improve use, is what makes recall() actually find what
    // bruh ingested instead of coming back empty against an unrelated dataset.
    let body = json!({
        "searchType": "GRAPH_COMPLETION",
        "query": query,
        "datasets": [super::DATASET_NAME],
    });
    let response = client.post("recall", body).await?;
    Ok(normalise_response(response))
}

/// Normalise various Cognee response shapes into a consistent `{ "text": "..." }` structure.
fn normalise_response(v: Value) -> Value {
    // COGNEE-017: the real /api/v1/recall (and /api/v1/search) response, confirmed against
    // docs.cognee.ai, is a JSON array of objects shaped like:
    //   [{ "search_result": <string|any>, "dataset_id": "...", "dataset_name": "..." }, ...]
    // This didn't match any of the shapes normalise_response used to check for ("text",
    // "result", "answer", "response", or array items with "text"/"content"), so a real
    // response always fell through to the raw pretty-printed JSON fallback, which is why
    // formatted output looked like a JSON dump instead of a clean timeline. Check this shape
    // first since it's the one Cognee actually sends.
    if let Some(arr) = v.as_array() {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(|item| item.get("search_result"))
            .filter_map(search_result_to_text)
            .filter(|s| !s.trim().is_empty())
            .collect();
        if !parts.is_empty() {
            return json!({ "text": parts.join("\n\n") });
        }
    }

    // Already has a "text" key, use it directly (kept as a fallback in case a
    // self-hosted tenant's response shape differs from Cognee Cloud's).
    if v.get("text").and_then(|t| t.as_str()).is_some() {
        return v;
    }

    // "result" key
    if let Some(s) = v.get("result").and_then(|t| t.as_str()) {
        return json!({ "text": s });
    }

    // "answer" key
    if let Some(s) = v.get("answer").and_then(|t| t.as_str()) {
        return json!({ "text": s });
    }

    // "response" key
    if let Some(s) = v.get("response").and_then(|t| t.as_str()) {
        return json!({ "text": s });
    }

    // Array of results with a "text"/"content" field (older/alternate shape)
    if let Some(arr) = v.as_array() {
        let combined: Vec<String> = arr
            .iter()
            .filter_map(|item| {
                item.get("text")
                    .or_else(|| item.get("content"))
                    .and_then(|t| t.as_str())
                    .map(|s| s.to_string())
            })
            .collect();
        if !combined.is_empty() {
            return json!({ "text": combined.join("\n\n") });
        }
    }

    // null / empty → helpful placeholder
    if v.is_null() {
        return json!({ "text": "" });
    }

    // Unknown structure, pretty print as fallback
    json!({ "text": serde_json::to_string_pretty(&v).unwrap_or_default() })
}

/// `search_result` is typed `any` in Cognee's schema, usually a string for
/// GRAPH_COMPLETION, but other search_types can return nested objects/arrays.
fn search_result_to_text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Null => None,
        other => serde_json::to_string_pretty(other).ok(),
    }
}

