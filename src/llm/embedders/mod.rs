/// Module for OpenAI embedder implementation
pub mod openai_embedder;

use async_trait::async_trait;
use std::error::Error;

pub use openai_embedder::*;

/// Trait defining interface for text embedding functionality
#[async_trait]
pub trait Embedder {
    /// Embeds the given text into a vector of floating point numbers
    ///
    /// # Arguments
    ///
    /// * `text` - The text to embed
    ///
    /// # Returns
    ///
    /// A Result containing either:
    /// * A vector of f32 values representing the embedding
    /// * A boxed Error if embedding fails
    async fn embed_text(&self, text: &str) -> Result<Vec<f32>, Box<dyn Error>>;
}
