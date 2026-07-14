//! The three LLM backends the discovery system can call on to figure out an unknown package
//! manager.
//!
//! Remember the cascade design from the memory: Gemini first, then Groq, then Claude, in
//! that order of preference (mostly because of free tier limits and speed, cheapest and
//! fastest options get tried first). Each backend implements the same ExtractorBackend trait
//! from extractor.rs so the calling code in extractor.rs doesn't need to care which one
//! actually answered, it just needs something that can extract a PackageManagerProfile from
//! search snippets.

mod claude;
mod gemini;
mod groq;

// Re-exporting the three backend structs here so the rest of the discovery module can just
// do `use crate::discovery::providers::GeminiBackend` instead of drilling into each file.
pub use claude::ClaudeBackend;
pub use gemini::GeminiBackend;
pub use groq::GroqBackend;

use std::sync::OnceLock;

static SHARED_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

// cognee/mod.rs made this exact fix already (see COGNEE-013 there): building a fresh
// reqwest::Client on every call means a fresh connection pool and a fresh TLS handshake
// every time, which is a real cost on a mobile connection. Discovery calls are rare enough
// (rate-limited, and usually one-shot per unknown manager) that this matters far less here
// than it did for cognee's every-30-seconds flush, but there's no reason to pay the cost at
// all when a single shared client works just as well and keeps this consistent with how the
// rest of the codebase already talks to the network.
/// The shared, process-wide `reqwest::Client` used by all three discovery backends.
pub(crate) fn http_client() -> &'static reqwest::Client {
    SHARED_CLIENT.get_or_init(reqwest::Client::new)
}
