//! This is the entry point for the cognee layer. As usual we first begin with the children declaration.

mod forget;
mod improve;
mod ingest;
mod query;
// We then call in the functions we need, yep, for public use via this API :-)
pub use forget::forget;
pub use improve::improve;
pub use ingest::{remember, remember_single};
pub use query::recall;

// Every single thing bruh sends to or asks Cognee for (add, cognify, recall, forget) needs
// to point at the exact same dataset, otherwise you get precisely the situation that
// prompted this constant to exist: one part of the code writing into "bruh_activity" while
// another part quietly falls back to whatever Cognee's own default happens to be, and now
// your queries look like they know nothing even though the data's sitting right there.
// Before this, "bruh_activity" was typed out by hand in four separate files. That's four
// chances for a typo or a copy-paste slip to silently split bruh's memory across two
// datasets. One constant, referenced everywhere, means there's only one place to ever
// change it, and it's structurally impossible for ingest and recall to drift apart again.
pub const DATASET_NAME: &str = "bruh_activity";

// For error propagation, we have this. 
// Usually we need it in every file that uses it.
use anyhow::{Context, Result};
// And for our json
use serde_json::Value;
use std::sync::OnceLock;
// 
pub struct CogneeClient {
    client: reqwest::Client,
    api_key: String,
    api_url: String,
}

/// COGNEE-013: a single process-wide CogneeClient instead of building a fresh one
/// (and a fresh reqwest::Client, meaning a fresh connection pool + TLS handshake)
/// on every remember()/recall()/improve()/forget() call. On a mobile connection
/// the handshake cost alone was a meaningful chunk of "why is this slow".
static SHARED_CLIENT: OnceLock<CogneeClient> = OnceLock::new();

/// COGNEE-014: GRAPH_COMPLETION-style queries route through an LLM over the graph
/// and can legitimately take a while, Cognee's own client integrations default to
/// a 5 minute timeout for exactly this reason. 30s (the old value) was cutting real
/// queries off mid-flight, which is what looked like "it just hangs, no reply".
/// remember() is comparatively fast, so it doesn't need this long a ceiling, but
/// giving every request the same generous timeout is simpler and safe, a request
/// that finishes in 2s doesn't wait around, this only bounds the worst case.
const REQUEST_TIMEOUT_SECS: u64 = 120;

// We write custom functions for the CogneeClient struct 
impl CogneeClient {
// This creates a new instance of it. It accepts the api_key and api url while attempting to build the cliemt from the builder with a check of 30 seconds.
    pub fn new(api_key: String, api_url: String) -> Self {
        Self {
// We'll talk more abou this line
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
                // COGNEE-013b: unwrap_or_default() used to silently fall back to a
                // bare reqwest::Client::default() with NO timeout at all if .build()
                // ever failed, meaning a request could hang forever with nothing
                // bounding it. .build() practically only fails on TLS backend
                // init issues, so if it does fail we want that to be loud, not a
                // silent switch to an unbounded client.
                .build()
                .expect("failed to build reqwest client (TLS backend init failed)"),
            api_key, // Struct Init Shorthand
            api_url, // Same here
        }
    }

    /// COGNEE-013c: returns the shared, process-wide client, building it from config
    /// on first use. Every cognee::* call site should go through this instead of
    /// CogneeClient::from_config() so the daemon (and any single CLI invocation that
    /// makes more than one call) reuses one connection pool.
    pub fn shared() -> Result<&'static CogneeClient> {
        if let Some(c) = SHARED_CLIENT.get() {
            return Ok(c);
        }
        let client = Self::from_config()?;
        let _ = SHARED_CLIENT.set(client);
        Ok(SHARED_CLIENT.get().expect("just set above"))
    }

    pub fn from_config() -> Result<Self> {
// We load values from the configuration file via the cli::Config::load function. Context is a method for error or more info for anyhow. This will return as a Struct we can access.
        let config = crate::cli::Config::load().context("Failed to load config")?;
// We access to find if there is an api key for cognee otherwise we bail it. Remember strictly that the anyhow bail has a return implementation in it so the program will crash with the error message. In this scenario, we check if it's empty.
        if config.cognee_api_key.is_empty() {
            anyhow::bail!(
                "Cognee API key is not set.\n\
                 Run 'bruh init' to configure it, or set the BRUH_COGNEE_API_KEY environment variable.\n\
                 Get a key at: https://app.cognee.ai"
            );
        }
// If it's not empty, we send the api key and api url to the new constructor function above, that will create a new CogneeClient and wrap it in Ok to be sent to the calling program.
        Ok(Self::new(config.cognee_api_key, config.cognee_api_url))
    }

    /// COGNEE-005: every real Cognee endpoint (self-hosted or Cognee Cloud) lives under
    /// the versioned `/api/v1/` prefix, e.g. `/api/v1/remember`, `/api/v1/recall`.
    /// Hitting the bare root (`{api_url}/{endpoint}`) 404s even against the correct host.
    fn build_url(&self, endpoint: &str) -> String {
        format!("{}/api/v1/{}", self.api_url.trim_end_matches('/'), endpoint)
    }

    /// COGNEE-006: Cognee Cloud tenants authenticate with `X-Api-Key: <key>`, not
    /// `Authorization: Bearer`. Bearer tokens are only for self-hosted instances after
    /// POST /api/v1/auth/login. We default to X-Api-Key since our default host
    /// (api.cognee.ai) and tenant subdomains (*.aws.cognee.ai) are both Cognee Cloud.
    fn auth_header_name(&self) -> &'static str {
        "X-Api-Key"
    }

    // COGNEE-015: was attempt < 2 (up to 2 retries = 3 attempts per call, each with
    // its own 1s/2s sleep). That was the *only* backoff in the system, do_flush()
    // and flush_buffered_events() each called through this on every tick with no
    // memory of prior failures, so a sustained outage meant every tick re-ran this
    // full retry ladder from scratch. Now that both flush paths check
    // buffer::should_retry() before attempting at all, this only needs to smooth
    // over brief blips, not carry an entire outage.
    //
    // COGNEE-018: bumped from 1 to 2. Here's why. A 409 from Cognee's pipeline registry
    // (see the 409 branch below) isn't always cleared up after a single short wait. If our
    // own call collided with a still-running cognify pipeline on the same dataset, that
    // pipeline might genuinely need another 10 to 30 seconds to finish, not 2. One extra
    // attempt, with a longer wait, gives a real, recoverable conflict a fair shot at
    // clearing before we give up and dump the batch into the offline buffer.
    const MAX_ATTEMPT_FOR_RETRY: u8 = 2;

    // Inspect an HTTP status and decide what the caller should do next.
    fn classify_status(status: reqwest::StatusCode, attempt: u8) -> StatusAction {
        if status.is_success() {
            StatusAction::Success
        } else if status.as_u16() == 401 || status.as_u16() == 403 {
            StatusAction::AuthError
        } else if status.is_server_error() && attempt < Self::MAX_ATTEMPT_FOR_RETRY {
            StatusAction::Retry
        } else if status.as_u16() == 409 && attempt < Self::MAX_ATTEMPT_FOR_RETRY {
            // COGNEE-018: this is the fix for the "409 errors" flakiness from notes.txt.
            // Cognee's own docs for /remember say plainly that it isn't a
            // PipelineRunErrored style 500 endpoint. Every failure inside it, including a
            // transient "there's already a pipeline run in progress for this dataset"
            // conflict, gets surfaced as a plain 409. Our pipeline_run_id for a given
            // dataset is deterministic (same user plus dataset plus pipeline name always
            // hashes to the same id), so if our daemon fires a second call while an
            // earlier one on the same dataset is still being processed server-side, we
            // collide with our own still-running job. That's not a real failure, it's bad
            // timing, and it clears up on its own once the earlier run finishes. Before
            // this fix we treated every 409 as instantly fatal (see the Fail branch
            // below), so a race we caused against ourselves looked identical to a genuine
            // error. Retrying gives the earlier run a chance to finish first.
            StatusAction::Retry
        } else {
            StatusAction::Fail
        }
    }

    // We play some kind of  JWT game around here.
    pub async fn post(&self, endpoint: &str, body: Value) -> Result<Value> {
        self.post_with_timeout(endpoint, body, None).await
    }

    /// COGNEE-021: same as post(), but lets a caller ask for a longer timeout than the
    /// shared client's default 120s. Cognify is the one call in this whole file that
    /// genuinely needs this, Cognee's own docs mention it can take up to 10 minutes on a
    /// larger dataset, since it's an LLM chewing through everything, not a quick database
    /// write. Every other call (add, recall, forget) is fine with the normal client
    /// timeout, so this stays opt-in rather than raising the ceiling for everything.
    pub async fn post_with_timeout(
        &self,
        endpoint: &str,
        body: Value,
        timeout: Option<Duration>,
    ) -> Result<Value> {
        let url = self.build_url(endpoint);

        for attempt in 0..3u8 {
            let mut req = self
                .client
                .post(&url)
                .header(self.auth_header_name(), &self.api_key)
                .header("Content-Type", "application/json")
                .json(&body); // Cognee accepts both strings and files so we send .json to it.

            if let Some(t) = timeout {
                req = req.timeout(t);
            }

            let resp = req.send().await.with_context(|| {
                format!(
                    "Network error reaching Cognee at {}.\n\
                     Check your internet connection or BRUH_COGNEE_API_URL.",
                    url
                )
            })?;
// We can now take the value of the response and do some stuff with it like:.. We'll test on Wednesday when the Cloud is live.
            let status = resp.status();
            match Self::classify_status(status, attempt) {
                StatusAction::Success => {
                    // We create some fallback if response isn't JSON
                    return resp
                        .json::<Value>()
                        .await
                        .or_else(|_| Ok::<_, anyhow::Error>(Value::Null));
                }
                StatusAction::AuthError => {
                    anyhow::bail!(
                        "Cognee API authentication failed (HTTP {}).\n\
                         Your API key may be invalid or expired.\n\
                         Run 'bruh config set cognee_api_key <new_key>' to update it.",
                        status
                    );
                }
                StatusAction::Retry => {
                    // COGNEE-018: a plain 2^attempt backoff makes sense for a genuine
                    // server error, we're just waiting out a hiccup. It doesn't make sense
                    // for a 409 pipeline conflict, because what we're actually waiting on
                    // is another pipeline run finishing, and that can take a good deal
                    // longer than 2 or 4 seconds. So 409s get their own, longer curve
                    // (10s, then 20s) instead of borrowing the network-hiccup one.
                    let wait = if status.as_u16() == 409 {
                        Duration::from_secs(10u64.saturating_mul(u64::from(attempt) + 1))
                    } else {
                        Duration::from_secs(2u64.pow(attempt as u32))
                    };
                    log::warn!("Cognee returned {}. Retrying in {:?}…", status, wait);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                StatusAction::Fail => {
                    let body_text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Cognee API error: HTTP {} — {}", status, body_text);
                }
            }
        }

        unreachable!() // this is reachable only if MAX_ATTEMPT_FOR_RETRY somehow exceeds the loop bound above, which the two constants are kept in sync on purpose to avoid.
    }

    /// COGNEE-007: /api/v1/remember is a multipart/form-data endpoint (it accepts raw
    /// text and/or file uploads plus batching form fields like chunks_per_batch), not
    /// a JSON body. This mirrors post() but sends a multipart::Form instead of .json().
    ///
    /// `build_form` is called fresh on every retry attempt since reqwest::multipart::Form
    /// is consumed on send and isn't Clone.
    pub async fn post_multipart<F>(&self, endpoint: &str, build_form: F) -> Result<Value>
    where
        F: Fn() -> reqwest::multipart::Form,
    {
        let url = self.build_url(endpoint);

        for attempt in 0..3u8 {
            let resp = self
                .client
                .post(&url)
                .header(self.auth_header_name(), &self.api_key)
                .multipart(build_form())
                .send()
                .await
                .with_context(|| {
                    format!(
                        "Network error reaching Cognee at {}.\n\
                     Check your internet connection or BRUH_COGNEE_API_URL.",
                        url
                    )
                })?;

            let status = resp.status();
            match Self::classify_status(status, attempt) {
                StatusAction::Success => {
                    return resp
                        .json::<Value>()
                        .await
                        .or_else(|_| Ok::<_, anyhow::Error>(Value::Null));
                }
                StatusAction::AuthError => {
                    anyhow::bail!(
                        "Cognee API authentication failed (HTTP {}).\n\
                         Your API key may be invalid or expired.\n\
                         Run 'bruh config set cognee_api_key <new_key>' to update it.",
                        status
                    );
                }
                StatusAction::Retry => {
                    // COGNEE-018: see the matching comment in post() above, same reasoning
                    // applies here, remember/add go through this multipart path and a 409
                    // here is the exact same "another pipeline run on this dataset is
                    // still busy" conflict, so it gets the same longer backoff curve.
                    let wait = if status.as_u16() == 409 {
                        Duration::from_secs(10u64.saturating_mul(u64::from(attempt) + 1))
                    } else {
                        Duration::from_secs(2u64.pow(attempt as u32))
                    };
                    log::warn!("Cognee returned {}. Retrying in {:?}…", status, wait);
                    tokio::time::sleep(wait).await;
                    continue;
                }
                StatusAction::Fail => {
                    let body_text = resp.text().await.unwrap_or_default();
                    anyhow::bail!("Cognee API error: HTTP {} — {}", status, body_text);
                }
            }
        }

        unreachable!() // same note as post() above: kept in sync with MAX_ATTEMPT_FOR_RETRY on purpose.
    }
}

enum StatusAction {
    Success,
    AuthError,
    Retry,
    Fail,
}

use std::time::Duration; // Let this be here for now. It doesn't hurt.		
