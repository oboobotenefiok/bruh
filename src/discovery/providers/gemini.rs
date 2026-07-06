// This is the Gemini backend for the discovery cascade. It's the first one we try because
// Google's free tier on gemini-1.5-flash is generous and the model is fast enough that it
// doesn't hold up the daemon for long when it hits an unknown package manager.
use crate::discovery::extractor::ExtractorBackend;
use crate::events::{Confidence, PackageManagerProfile};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
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
        // shaped so we go with it.
        let client = reqwest::Client::new();
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

// This is shared logic (well, duplicated across the three provider files really, I know
// that's not DRY but each provider's response shape is different enough going in that it
// felt more readable to keep it separate for now than to abstract too early).
// It takes whatever text the model spat out, finds the first '{' and last '}' to strip
// away any stray prose the model added despite our instructions, then parses what's left
// as JSON and maps it onto our PackageManagerProfile struct.
fn build_profile(name: &str, text: &str, provider: &str) -> Result<PackageManagerProfile> {
    let start = text.find('{').unwrap_or(0);
    let end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
    let v: Value = serde_json::from_str(&text[start..end])
        .context("Failed to parse JSON from provider response")?;

    Ok(PackageManagerProfile {
        node_type: "PackageManagerProfile".into(),
        name: name.to_string(),
        log_path: v["log_path"].as_str().map(|s| s.to_string()),
        registry_path: v["registry_path"].as_str().map(|s| s.to_string()),
        // We fall back to sensible defaults (install/remove/list) if the model somehow
        // didn't return a field, better to have a guess than to fail the whole discovery.
        install_verb: v["install_verb"].as_str().unwrap_or("install").to_string(),
        remove_verb: v["remove_verb"].as_str().unwrap_or("remove").to_string(),
        list_command: v["list_command"].as_str().unwrap_or("list").to_string(),
        discovered_at: Utc::now(),
        confidence: match v["confidence"].as_str().unwrap_or("medium") {
            "high" => Confidence::High,
            "low" => Confidence::Low,
            _ => Confidence::Medium,
        },
        first_seen_command: format!("{} install <package>", name),
        discovered_by_provider: Some(provider.to_string()),
    })
}
