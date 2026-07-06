use super::CogneeClient;
use anyhow::Result;
use serde_json::json;

pub async fn forget(before: Option<String>, session: Option<String>) -> Result<()> {
    // COGNEE-013: shared client, see cognee/mod.rs.
    let client = CogneeClient::shared()?;
    // Here's the thing this was missing. Every other cognee operation we make, add in
    // ingest.rs, cognify in improve.rs, and recall in query.rs, all point at the same
    // DATASET_NAME constant so they're all working with the same slice of data. This
    // function never did that, it left the dataset field out entirely. The COGNEE-009
    // note below already tells us the confirmed schema includes a dataset field, so
    // leaving it out isn't "no dataset," it's "whatever Cognee falls back to when you
    // don't say," which on a tenant that has more than one dataset is exactly the kind
    // of quiet mismatch that makes forget look like it did nothing, or worse, makes it
    // touch data outside what bruh actually owns. Referencing the shared constant here
    // brings forget in line with the other three, so the whole cognee layer is finally
    // reading the same one dataset name from the same one place.
    let mut body = json!({ "dataset": super::DATASET_NAME });
    // COGNEE-009: the confirmed /api/v1/forget schema is {dataset, session_id, memory_only, ...}.
    // `session` maps cleanly onto the documented session_id field. `before` (a date cutoff)
    // has no confirmed matching field in Cognee's public schema, there is no documented
    // date-range delete. It's passed through as-is so it isn't silently dropped, but this
    // needs verifying against the tenant's Swagger/OpenAPI page before relying on it.
    if let Some(b) = before {
        body["before"] = json!(b);
    }
    if let Some(s) = session {
        body["session_id"] = json!(s);
    }
    client.post("forget", body).await?;
    Ok(())
}
