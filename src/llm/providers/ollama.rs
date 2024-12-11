use super::LlmProvider;
use crate::llm::ChatMessage;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::error::Error;

/// Provider implementation for Ollama's local API
#[derive(Debug)]
pub struct OllamaProvider {
    /// Model identifier to use (e.g. "llama2", "codellama")
    model: String,
}

impl OllamaProvider {
    /// Creates a new Ollama provider instance
    ///
    /// # Arguments
    /// * `model` - The model identifier to use
    ///
    /// # Returns
    /// * `Result<Self, Box<dyn Error>>` - Provider instance
    pub fn new(model: &str) -> Result<Self, Box<dyn Error>> {
        Ok(OllamaProvider {
            model: model.to_string(),
        })
    }
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    /// Calls Ollama's chat API
    ///
    /// # Arguments
    /// * `system_prompt` - System message to set context/behavior
    /// * `user_prompt` - User's input message
    ///
    /// # Returns
    /// * `Result<String, Box<dyn Error>>` - Generated response text or error
    async fn call_llm_api(&self, messages: Vec<ChatMessage>) -> Result<String, Box<dyn Error>> {
        let client = Client::new();

        let request_body = json!({
            "model": self.model,
            "stream": false,
            "messages": messages
        });

        let res = client
            .post("http://localhost:11400/api/chat")
            .json(&request_body)
            .send()
            .await?;

        if !res.status().is_success() {
            let text = res.text().await?;
            return Err(format!("Ollama API error: {}", text).into());
        }

        let json_resp: serde_json::Value = res.json().await?;
        if let Some(content) = json_resp["message"]["content"].as_str() {
            Ok(content.trim().to_string())
        } else {
            Err("No content in Ollama LLM response".into())
        }
    }
}
