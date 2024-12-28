use super::LlmProvider;
use crate::llm::ChatMessage;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::error::Error;

/// Provider implementation for DeepSeek's API
#[derive(Debug)]
pub struct DeepSeekProvider {
    /// API key loaded from environment
    api_key: String,
    /// Model identifier to use (e.g. "deepseek-chat")
    model: String,
}

impl DeepSeekProvider {
    /// Creates a new DeepSeek provider instance
    ///
    /// # Arguments
    /// * `model` - The model identifier to use
    ///
    /// # Returns
    /// * `Result<Self, Box<dyn Error>>` - Provider instance or error if API key not found
    pub fn new(model: &str) -> Result<Self, Box<dyn Error>> {
        let api_key = std::env::var("DEEPSEEK_API_KEY")
            .map_err(|_| "DEEPSEEK_API_KEY environment variable not set")?;
        Ok(DeepSeekProvider {
            api_key,
            model: model.to_string(),
        })
    }
}

#[async_trait]
impl LlmProvider for DeepSeekProvider {
    /// Calls DeepSeek's chat completions API
    ///
    /// # Arguments
    /// * `messages` - Vector of chat messages containing the conversation history
    ///
    /// # Returns
    /// * `Result<String, Box<dyn Error>>` - Generated response text or error
    async fn call_llm_api(&self, messages: Vec<ChatMessage>) -> Result<String, Box<dyn Error>> {
        let client = Client::new();
        let request_body = json!({
          "model": self.model,
          "messages": messages,
          "temperature": 0.7,
          "stream": false
        });

        let res = client
            .post("https://api.deepseek.com/chat/completions")
            .bearer_auth(&self.api_key)
            .json(&request_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let text = res.text().await?;
            return Err(format!("DeepSeek API error: {}", text).into());
        }

        let json_resp: serde_json::Value = res.json().await?;
        if let Some(content) = json_resp["choices"][0]["message"]["content"].as_str() {
            Ok(content.trim().to_string())
        } else {
            Err("No content in DeepSeek LLM response".into())
        }
    }
}
