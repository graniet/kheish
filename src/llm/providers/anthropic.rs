use super::LlmProvider;
use crate::llm::ChatMessage;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::error::Error;
use tracing::debug;

/// Provider implementation for Anthropic's API
#[derive(Debug)]
pub struct AnthropicProvider {
    /// Anthropic API key loaded from environment
    api_key: String,
    /// Model identifier to use (e.g. "claude-2", "claude-instant-1")
    model: String,
}

impl AnthropicProvider {
    /// Creates a new Anthropic provider instance
    ///
    /// # Arguments
    /// * `model` - The model identifier to use
    ///
    /// # Returns
    /// * `Result<Self, Box<dyn Error>>` - Provider instance or error if API key not found
    pub fn new(model: &str) -> Result<Self, Box<dyn Error>> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .map_err(|_| "ANTHROPIC_API_KEY environment variable not set")?;
        Ok(AnthropicProvider {
            api_key,
            model: model.to_string(),
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    /// Calls Anthropic's messages API
    ///
    /// # Arguments
    /// * `system_prompt` - System message to set context/behavior
    /// * `user_prompt` - User's input message
    ///
    /// # Returns
    /// * `Result<String, Box<dyn Error>>` - Generated response text or error
    async fn call_llm_api(&self, messages: Vec<ChatMessage>) -> Result<String, Box<dyn Error>> {
        let client = Client::new();

        let (system_messages, user_messages): (Vec<_>, Vec<_>) =
            messages.into_iter().partition(|msg| msg.role == "system");
        let system_content = system_messages
            .into_iter()
            .map(|m| m.content)
            .collect::<Vec<_>>()
            .join("\n");
        let messages = user_messages
            .into_iter()
            .map(|msg| ChatMessage::new(&msg.role, &msg.content))
            .collect::<Vec<_>>();

        let request_body = json!({
            "model": self.model,
            "system": system_content,
            "max_tokens": 4096,
            "messages": messages
        });

        let res = client
            .post("https://api.anthropic.com/v1/messages")
            .header("x-api-key", self.api_key.to_string())
            .header("anthropic-version", "2023-06-01")
            .json(&request_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let text = res.text().await?;
            return Err(format!("Anthropic API error: {}", text).into());
        }

        let json_resp: serde_json::Value = res.json().await?;
        if let Some(content) = json_resp["content"][0]["text"].as_str() {
            debug!("Anthropic response: {}", content);
            Ok(content.trim().to_string())
        } else {
            Err("No content in Anthropic LLM response".into())
        }
    }
}
