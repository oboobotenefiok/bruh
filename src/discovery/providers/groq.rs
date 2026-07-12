// Second backend in the discovery cascade, tried after Gemini fails or isn't configured.
// Groq is running Llama here, and their inference is genuinely fast, which matters because
// this whole extraction step happens while the daemon is otherwise busy watching for events,
// we don't want a slow LLM call to become a bottleneck.
use crate::{
    discovery::extractor::{build_profile, ExtractorBackend},
    events::PackageManagerProfile,
};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::Value;

// CONFIG-003: resolved key handed in at construction time (config value first, GROQ_API_KEY
// env var as fallback), same pattern as GeminiBackend, see gemini.rs for the longer version
// of this comment.
pub struct GroqBackend {
    pub api_key: Option<String>,
}

#[async_trait]
impl ExtractorBackend for GroqBackend {
    fn name(&self) -> &'static str {
        "groq"
    }

    // Same pattern as Gemini's backend, if the key isn't set we tell the cascade to skip us.
    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        let api_key = self.api_key.clone().context(
            "No Groq key configured. Run `bruh config set groq_api_key <key>` \
             or export GROQ_API_KEY.",
        )?;

        // DISCOVERY-008: Groq/Llama works best with concise, direct prompts
        // Unlike Gemini, Llama doesn't need as much hand holding about markdown fences,
        // it tends to just answer with the JSON when you ask directly. Still, I keep the
        // "Reply ONLY with JSON" instruction because it costs nothing and helps a bit.
        let prompt = format!(
            "Extract package manager info for '{}' from these snippets. \
             Reply ONLY with JSON: {{\"install_verb\":\"...\",\"remove_verb\":\"...\",\
             \"list_command\":\"...\",\"log_path\":null,\"registry_path\":null,\
             \"confidence\":\"high|medium|low\"}}\nSnippets:\n{}",
            manager_name,
            snippets.join("\n")
        );

        // Groq mimics the OpenAI chat completions API shape, which is convenient because
        // it means this request body looks familiar if you've used OpenAI's SDK before.
        let client = super::http_client();
        let resp = client
            .post("https://api.groq.com/openai/v1/chat/completions")
            .header("Authorization", format!("Bearer {}", api_key))
            .json(&serde_json::json!({
                "model": "llama-3.3-70b-versatile",
                "messages": [{"role": "user", "content": prompt}],
                "temperature": 0.1,
                "max_tokens": 256
            }))
            .send()
            .await
            .context("Groq request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "Groq HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        // OpenAI-shaped responses nest the actual reply under choices[0].message.content,
        // that's what we're pulling out here.
        let json: Value = resp.json().await.context("Groq response not JSON")?;
        let text = json["choices"][0]["message"]["content"]
            .as_str()
            .context("Groq: no content in response")?;

        build_profile(manager_name, text, "groq")
    }
}

