use super::Embedder;
use async_trait::async_trait;
use reqwest::Client;
use serde_json::json;
use std::error::Error;

/// OpenAI embedder implementation that uses OpenAI's API to generate text embeddings
#[derive(Debug)]
pub struct OpenAIEmbedder {
    /// OpenAI API key used for authentication
    pub api_key: String,
    /// Name of the OpenAI model to use for embeddings
    pub model: String,
}

impl OpenAIEmbedder {
    /// Creates a new OpenAIEmbedder instance
    ///
    /// # Arguments
    ///
    /// * `model` - Name of the OpenAI model to use
    ///
    /// # Returns
    ///
    /// A Result containing either:
    /// * A new OpenAIEmbedder instance
    /// * An error if the OPENAI_API_KEY environment variable is not set
    pub fn new(model: &str) -> Result<Self, Box<dyn Error>> {
        let api_key = std::env::var("OPENAI_API_KEY")
            .map_err(|_| "OPENAI_API_KEY environment variable not set")?;
        Ok(Self {
            api_key,
            model: model.to_string(),
        })
    }
}

#[async_trait]
impl Embedder for OpenAIEmbedder {
    /// Embeds the given text using OpenAI's API
    ///
    /// # Arguments
    ///
    /// * `text` - The text to embed
    ///
    /// # Returns
    ///
    /// A Result containing either:
    /// * A vector of f32 values representing the embedding
    /// * An error if the API call fails or returns invalid data
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, Box<dyn Error>> {
        let client = Client::new();
        let body = json!({
            "input": text,
            "model": self.model
        });

        let res = client
            .post("https://api.openai.com/v1/embeddings")
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?;

        if !res.status().is_success() {
            let txt = res.text().await?;
            return Err(format!("Error from OpenAI: {}", txt).into());
        }

        let json_resp: serde_json::Value = res.json().await?;
        let arr = json_resp["data"][0]["embedding"]
            .as_array()
            .ok_or("No embedding")?;
        let embedding: Vec<f32> = arr
            .iter()
            .filter_map(|x| x.as_f64())
            .map(|x| x as f32)
            .collect();
        Ok(embedding)
    }
}
