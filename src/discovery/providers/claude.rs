// Last resort in the discovery cascade. Claude usually gives the most reliable structured
// output of the three, honestly, but it's tried last mainly because of cost and the fact
// that most people won't have ANTHROPIC_API_KEY set specifically for this side feature when
// they're already using it elsewhere. If Gemini and Groq both fail or aren't configured,
// this is the safety net.
use crate::discovery::extractor::ExtractorBackend;
use crate::events::{Confidence, PackageManagerProfile};
use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use serde_json::Value;

// CONFIG-003: resolved key handed in at construction time (config value first,
// ANTHROPIC_API_KEY env var as fallback), same pattern as GeminiBackend.
pub struct ClaudeBackend {
    pub api_key: Option<String>,
}

#[async_trait]
impl ExtractorBackend for ClaudeBackend {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn is_available(&self) -> bool {
        self.api_key.is_some()
    }

    async fn extract(
        &self,
        manager_name: &str,
        snippets: &[String],
    ) -> Result<PackageManagerProfile> {
        let api_key = self.api_key.clone().context(
            "No Claude key configured. Run `bruh config set claude_api_key <key>` \
             or export ANTHROPIC_API_KEY.",
        )?;

        // DISCOVERY-008: Claude responds well to explicit structure instructions
        // I give it a literal example of the JSON shape here rather than just describing
        // the fields in prose, Claude tends to follow a shown structure very closely.
        let prompt = format!(
            "Extract package manager metadata for '{}' from the following search snippets.\n\n\
             Return ONLY a JSON object with this exact structure (no markdown, no prose):\n\
             {{\n  \"install_verb\": \"install\",\n  \"remove_verb\": \"remove\",\n\
             \"list_command\": \"list\",\n  \"log_path\": null,\n  \"registry_path\": null,\n\
             \"confidence\": \"high\"\n}}\n\n\
             Snippets:\n{}",
            manager_name,
            snippets.join("\n")
        );

        // Anthropic's Messages API wants the key in an x-api-key header (not Authorization:
        // Bearer like Groq), plus an explicit anthropic-version header. Small detail but it
        // trips people up the first time they wire this up.
        let client = reqwest::Client::new();
        let resp = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", &api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "claude-haiku-4-5-20251001",
                "max_tokens": 512,
                "messages": [{"role": "user", "content": prompt}]
            }))
            .send()
            .await
            .context("Claude request failed")?;

        if !resp.status().is_success() {
            anyhow::bail!(
                "Claude HTTP {}: {}",
                resp.status(),
                resp.text().await.unwrap_or_default()
            );
        }

        // Claude's response puts the text under content[0].text, a flatter shape than
        // Gemini's nesting, which is one of the small reasons I like working with it.
        let json: Value = resp.json().await.context("Claude response not JSON")?;
        let text = json["content"][0]["text"]
            .as_str()
            .context("Claude: no text in response")?;

        build_profile(manager_name, text, "claude")
    }
}

fn build_profile(name: &str, text: &str, provider: &str) -> Result<PackageManagerProfile> {
    let start = text.find('{').unwrap_or(0);
    let end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
    let v: Value = serde_json::from_str(&text[start..end])
        .context("Failed to parse JSON from Claude response")?;

    Ok(PackageManagerProfile {
        node_type: "PackageManagerProfile".into(),
        name: name.to_string(),
        log_path: v["log_path"].as_str().map(|s| s.to_string()),
        registry_path: v["registry_path"].as_str().map(|s| s.to_string()),
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
