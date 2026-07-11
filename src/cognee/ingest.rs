//! COGNEE-002: chunked batches (max 500 events per request).
// This is the write side of the cognee layer, everything that ends up in Cognee's memory
// graph flows through remember() at some point, whether that's the daemon's regular flush,
// a buffer replay after an outage, or a single git commit event sent immediately. The other
// three cognee submodules (query, improve, forget) are all reads or graph operations,
// this one's the only place we actually push new data in.

use super::CogneeClient;
use crate::events::Event;
use anyhow::{Context, Result};
use log::debug;

const CHUNK_SIZE: usize = 500;

/// COGNEE-019: this used to POST to "remember", which sounds like the obvious choice for
/// a function named remember(), but it isn't the right endpoint for what the daemon is
/// doing here. Cognee's own docs spell out what /api/v1/remember actually does under the
/// hood: add + cognify + (by default) improve, all run synchronously, in one blocking
/// call. cognify is the expensive part, it's the LLM-driven graph extraction step, and it
/// can legitimately take well over a minute on a growing dataset. Our daemon flushes on a
/// timer (batch_flush_interval_seconds), so calling /remember from the daemon meant every
/// single flush blocked the daemon's main loop for however long a full graph rebuild
/// happened to take that time.
///
/// Worse, Cognee computes the pipeline_run_id for a dataset deterministically (same user,
/// same dataset, same pipeline name always hashes to the same id), so if one flush's
/// cognify step was still running server-side when the next flush's timer fired, the
/// second call collided with the first under that same id. Cognee doesn't hand back a
/// clean "still busy" response for that, it surfaces as a plain 409 (see the COGNEE-018
/// note in cognee/mod.rs), which is the exact flakiness described in notes.txt.
///
/// /api/v1/add is the lower-level primitive /remember composes: pure ingest, no cognify,
/// no improve. It returns fast because there's no LLM graph-build attached to it at all.
/// The daemon now uses this for its regular ticking flush, and the actual graph-build
/// step is triggered separately and much less often by daemon::mod's improve trigger
/// (see COGNEE-020 there), so ingest cadence and graph-build cadence are no longer forced
/// to be the same number.
pub async fn remember(events: Vec<Event>) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    // COGNEE-013: shared client, see cognee/mod.rs. Avoids rebuilding a reqwest::Client
    // (and reloading Config from disk) on every remember() call.
    let client = CogneeClient::shared()?;

    let chunks: Vec<&[Event]> = events.chunks(CHUNK_SIZE).collect();
    let total_chunks = chunks.len();
    if total_chunks > 1 {
        debug!(
            "remember(): sending {} events across {} chunks of up to {}",
            events.len(),
            total_chunks,
            CHUNK_SIZE
        );
    }

    for (i, chunk) in chunks.into_iter().enumerate() {
        // Serialize events as structured text that Cognee's graph builder can process.
        let text_blocks: Vec<String> = chunk.iter().map(event_to_text).collect();

        // COGNEE-007: /api/v1/add (like /api/v1/remember before it) is multipart/form-data,
        // not JSON. Each text block goes in as a repeated "data" field, alongside the
        // dataset name the daemon writes activity history into.
        //
        // COGNEE-011: the "data" field is typed as UploadFile server-side, which
        // FastAPI only recognises when the multipart part has a filename in its
        // Content-Disposition header. Form::text() sends a plain field (name only)
        // and gets rejected with "Expected UploadFile, received: <class 'str'>".
        // Using Part::text().file_name(...) makes reqwest include the filename,
        // so the part is treated as an uploaded file instead of a form string.
        client
            .post_multipart("add", {
                let text_blocks = text_blocks.clone();
                move || {
                    let mut form =
                        reqwest::multipart::Form::new().text("datasetName", super::DATASET_NAME);
                    for (i, block) in text_blocks.iter().enumerate() {
                        let part = reqwest::multipart::Part::text(block.clone())
                            .file_name(format!("event_{i}.txt"))
                            .mime_str("text/plain")
                            .expect("static mime type is always valid");
                        form = form.part("data", part);
                    }
                    form
                }
            })
            .await
            // BUFFER-006: without this context, a chunk failure just surfaces as one
            // generic error for the whole call, no way to tell from the log which
            // chunk it was or how many had already gone through. On a big replay
            // (the offline buffer can hold thousands of events) that ambiguity is
            // exactly the kind of thing that leads to guessing instead of reading it
            // straight off the log.
            .with_context(|| {
                format!(
                    "chunk {}/{} failed ({} events in this chunk, {} sent successfully before it)",
                    i + 1,
                    total_chunks,
                    chunk.len(),
                    i * CHUNK_SIZE
                )
            })?;

        if total_chunks > 1 {
            debug!("remember(): chunk {}/{} sent", i + 1, total_chunks);
        }
    }
    Ok(())
}

pub async fn remember_single(event: Event) -> Result<()> {
    remember(vec![event]).await
}

/// Convert an Event into a structured text block for Cognee ingestion.
fn event_to_text(event: &Event) -> String {
    match event {
        crate::events::Event::ShellCommand(e) => format!(
            "EVENT: shell_command\nTIMESTAMP: {}\nDIRECTORY: {}\nCOMMAND: {}\nEXIT_CODE: {}\nSESSION_ID: {}\nERROR_TYPE: {}\nOUTPUT: {}",
            e.timestamp.to_rfc3339(),
            e.directory,
            e.command,
            e.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()),
            e.session_id.as_deref().unwrap_or("unknown"),
            e.error_type.as_deref().unwrap_or("none"),
            e.output.as_deref().unwrap_or(""),
        ),
        crate::events::Event::PackageInstall(e) => format!(
            "EVENT: package_install\nTIMESTAMP: {}\nMANAGER: {}\nPACKAGE: {}\nVERSION: {}\nTRIGGER: {}\nSESSION_ID: {}\nDIRECTORY: {}",
            e.timestamp.to_rfc3339(),
            e.manager,
            e.package,
            e.version.as_deref().unwrap_or("unknown"),
            e.trigger_command.as_deref().unwrap_or("unknown"),
            e.session_id.as_deref().unwrap_or("unknown"),
            e.working_directory.as_deref().unwrap_or("unknown"),
        ),
        crate::events::Event::GitCommit(e) => format!(
            "EVENT: git_commit\nTIMESTAMP: {}\nHASH: {}\nMESSAGE: {}\nBRANCH: {}\nFILES: {}\nDIFF: {}\nSESSION_ID: {}\nDIRECTORY: {}",
            e.timestamp.to_rfc3339(),
            e.hash,
            e.message,
            e.branch,
            e.files_changed.join(", "),
            e.diff_summary.as_deref().unwrap_or(""),
            e.session_id.as_deref().unwrap_or("unknown"),
            e.working_directory.as_deref().unwrap_or("unknown"),
        ),
        crate::events::Event::PackageManagerProfile(p) => format!(
            "EVENT: package_manager_profile\nNAME: {}\nINSTALL_VERB: {}\nLIST: {}\nCONFIDENCE: {}\nPROVIDER: {}",
            p.name, p.install_verb, p.list_command, p.confidence,
            p.discovered_by_provider.as_deref().unwrap_or("unknown"),
        ),
    }
}

