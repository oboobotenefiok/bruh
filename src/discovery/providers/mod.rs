// src/discovery/providers/mod.rs
//
// This folder holds the three LLM backends the discovery system can call on to figure out
// an unknown package manager. Remember the cascade design from the memory: Gemini first,
// then Groq, then Claude, in that order of preference (mostly because of free tier limits
// and speed, cheapest and fastest options get tried first). Each backend implements the
// same ExtractorBackend trait from extractor.rs so the calling code in extractor.rs doesn't
// need to care which one actually answered, it just needs something that can extract a
// PackageManagerProfile from search snippets.

mod claude;
mod gemini;
mod groq;

// Re-exporting the three backend structs here so the rest of the discovery module can just
// do `use crate::discovery::providers::GeminiBackend` instead of drilling into each file.
pub use claude::ClaudeBackend;
pub use gemini::GeminiBackend;
pub use groq::GroqBackend;
