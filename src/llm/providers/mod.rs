use crate::llm::ChatMessage;
use async_trait::async_trait;
use std::error::Error;
use std::fmt::Debug;

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod deepseek;

#[async_trait]
pub trait LlmProvider: Debug + Send + Sync {
    async fn call_llm_api(&self, messages: Vec<ChatMessage>) -> Result<String, Box<dyn Error>>;
}
