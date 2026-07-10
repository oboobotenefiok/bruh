// This is the Gemini backend for the discovery cascade. It's the first one we try because
// Google's free tier on gemini-1.5-flash is generous and the model is fast enough that it
// doesn't hold up the daemon for long when it hits an unknown package manager.
use crate::{
    discovery::extractor::{build_profile, ExtractorBackend},
    events::PackageManagerProfile,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

// CONFIG-003: used to be a zero-sized struct that reached into env::var() directly. Now
// the cascade resolves the key once (config value first, GOOGLE_AI_API_KEY env var as
// fallback, see Config::resolved_gemini_key) and hands it to us here at construction time.
// That's the only thing that changed, is_available()/extract() below just read the field
// instead of calling env::var() themselves.
pub struct GeminiBackend {
    pub api_key: Option<String>,
}

// async_trait is one of the few "heavier" crates I let myself keep, because Rust doesn't
// support async fn in traits natively without it (well, not with dyn dispatch anyway, and
// I need dyn dispatch here since extractor.rs will hold a Vec of different backend types).
#[async_trait]
impl ExtractorBackend for GeminiBackend {
    fn name(&self) -> &'static str {
        "gemini"
    }

    // The cascade in extractor.rs calls is_available() on each backend before trying it,
    // so if the person hasn't configured a Gemini key (via `bruh config set gemini_api_key`
    // or the GOOGLE_AI_API_KEY env var) we just skip straight to Groq instead of wasting a
    // request that's guaranteed to fail.
    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        let api_key = self.api_key.clone().context(
            "No Gemini key configured. Run `bruh config set gemini_api_key <key>` \
             or export GOOGLE_AI_API_KEY.",
        )?;

        // DISCOVERY-008: Gemini needs explicit JSON output instruction
        // I learned this the hard way, if you don't explicitly say "no markdown, no code
        // fences" Gemini loves to wrap its JSON in ```json blocks which then breaks the
        // parser below. Being blunt about the format fixes it almost every time.
        let prompt = format!(
            "You are a developer tool. Extract information about the '{}' package manager from these search snippets.\n\
             Return ONLY valid JSON (no markdown, no code fences, no explanation) with exactly these fields:\n\
             {{\"install_verb\":\"...\",\"remove_verb\":\"...\",\"list_command\":\"...\",\
             \"log_path\":null,\"registry_path\":null,\"confidence\":\"high|medium|low\"}}\n\n\
             Search snippets:\n{}",
            manager_name,
            snippets.join("\n")
        );

        // Gemini's REST API takes the key as a query param rather than a header, which is a
        // bit unusual compared to the other two providers, but that's just how their API is
        // shaped so we go with it. Worth knowing if you ever add request logging here: the
        // key lives in the URL for this one provider, so never log the full request URL for
        // a Gemini call, unlike Claude/Groq's header-based auth, that would put the raw key
        // straight into daemon.log in plaintext.
        let client = super::http_client();
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-1.5-flash:generateContent?key={}",
            api_key
        );

        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "contents": [{"parts": [{"text": prompt}]}],
                "generationConfig": {"temperature": 0.1, "maxOutputTokens": 512}
            }))
            .send()
            .await
            .context("Gemini request failed")?;

        // If the HTTP status itself isn't a success, we bail early with the body attached
        // so whoever's debugging this later (probably me) can see exactly what Google sent back.
        if !resp.status().is_success() {
            anyhow::bail!(
                "Gemini HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        // Gemini nests the actual generated text pretty deep in its response shape, hence
        // the chain of index accesses below.
        let json: Value = resp.json().await.context("Gemini response not JSON")?;
        let text = json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .context("Gemini: no text in response")?;

        build_profile(manager_name, text, "gemini")
    }
}
