use crate::llm::providers::LlmProvider;
use crate::llm::ChatMessage;
use std::error::Error;
use tracing::{debug, info};
use crate::utils::manage_token_count;

/// Generic LLM client that delegates work to a concrete provider.
#[derive(Debug)]
pub struct LlmClient {
    provider: Box<dyn LlmProvider>,
}

impl LlmClient {
    /// Creates a new LLM client with the specified provider and model.
    ///
    /// # Arguments
    /// * `provider_name` - Name of the LLM provider ("openai", "anthropic", or "ollama")
    /// * `model` - Model name to use with the provider
    ///
    /// # Returns
    /// * `Result<LlmClient, Box<dyn Error>>` - New LLM client instance or error
    pub fn new(provider_name: &str, model: &str) -> Result<Self, Box<dyn Error>> {
        let provider: Box<dyn LlmProvider> = match provider_name {
            "openai" => Box::new(crate::llm::providers::openai::OpenAiProvider::new(model)?),
            "anthropic" => Box::new(crate::llm::providers::anthropic::AnthropicProvider::new(
                model,
            )?),
            "ollama" => Box::new(crate::llm::providers::ollama::OllamaProvider::new(model)?),
            _ => return Err(format!("Unknown provider '{}'", provider_name).into()),
        };

        Ok(LlmClient { provider })
    }

    /// Calls the LLM with system and user prompts and returns the raw response.
    ///
    /// # Arguments
    /// * `system_prompt` - System prompt to set context/behavior
    /// * `user_prompt` - User's input prompt
    ///
    /// # Returns
    /// * `Result<String, Box<dyn Error>>` - LLM response text or error
    pub async fn call_llm_api(&self, messages: Vec<ChatMessage>) -> Result<String, Box<dyn Error>> {
        self.provider.call_llm_api(messages).await
    }

    /// Calls the LLM with format validation and automatic retries if format check fails.
    ///
    /// # Arguments
    /// * `system_prompt` - System prompt to set context/behavior
    /// * `user_prompt` - User's input prompt
    /// * `validate_response` - Function to validate response format
    /// * `format_reminder` - Format instructions to include in retry attempts
    /// * `max_retries` - Maximum number of retry attempts
    ///
    /// # Returns
    /// * `Result<String, Box<dyn Error>>` - Validated LLM response or error
    pub async fn call_llm_with_format_check<F>(
        &self,
        messages: &mut Vec<ChatMessage>,
        validate_response: F,
        format_reminder: &str,
        max_retries: usize,
    ) -> Result<String, Box<dyn Error>>
    where
        F: Fn(&str) -> bool,
    {
        let mut attempts = 0;
        
        manage_token_count(messages, 35000);

        loop {
            attempts += 1;
            let response = self.call_llm_api(messages.clone()).await?;
            debug!("messages: {:?}", messages);
            debug!("LLM response: {}", response);

            if validate_response(&response) {
                return Ok(response);
            } else if attempts >= max_retries {
                info!(
                    "LLM did not follow the format after {} attempts response: {}",
                    max_retries, response
                );
                return Err(format!(
                    "LLM did not follow the format after {} attempts",
                    max_retries
                )
                .into());
            } else {
                let retry_message = format!(
                    "Your last answer did not follow the required format.\n\
                     {} \n\
                     Please provide a new answer following exactly these formatting rules.",
                    format_reminder
                );
                messages.push(ChatMessage::new("user", &retry_message));
            }
        }
    }
}
