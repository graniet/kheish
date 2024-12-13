use crate::llm::Embedder;
use async_trait::async_trait;
use std::error::Error;
use std::fmt::Debug;
use tracing::info;

/// A document with its embedding vector representation
#[derive(Clone, Debug)]
pub struct DocumentEmbedding {
    /// Unique identifier for the document
    #[allow(unused)]
    pub id: String,
    /// Vector embedding of the document content
    pub embedding: Vec<f32>,
    /// Original text content of the document
    pub content: String,
    /// Metadata about the document
    pub metadata: Option<String>,
}

/// Trait defining operations for a vector store
#[async_trait]
pub trait VectorStoreProvider: Send + Sync {
    /// Adds a new document to the store
    ///
    /// # Arguments
    /// * `content` - Text content of the document to add
    ///
    /// # Returns
    /// * `Result<String, Box<dyn Error>>` - ID of the added document or error
    async fn add_document(&mut self, content: &str) -> Result<String, Box<dyn Error>>;

    /// Searches for similar documents using vector similarity
    ///
    /// # Arguments
    /// * `query` - Text to search for
    /// * `top_k` - Maximum number of results to return
    ///
    /// # Returns
    /// * `Result<Vec<DocumentEmbedding>, Box<dyn Error>>` - Matching documents or error
    async fn search_documents(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<DocumentEmbedding>, Box<dyn Error>>;

    /// Upserts a document with the given ID and content
    ///
    /// # Arguments
    /// * `doc_id` - Unique identifier for the document
    /// * `content` - Text content of the document
    /// * `metadata` - Optional metadata about the document
    ///
    /// # Returns
    /// * `Result<(), Box<dyn Error>>` - Success or error
    async fn upsert_document(
        &mut self,
        doc_id: &str,
        content: &str,
        metadata: Option<String>,
    ) -> Result<(), Box<dyn Error>>;
}

/// In-memory implementation of a vector store
#[derive(Debug)]
pub struct InMemoryVectorStore<E: Debug + Embedder + Send + Sync> {
    /// Stored documents with their embeddings
    documents: Vec<DocumentEmbedding>,
    /// Counter for generating document IDs
    next_id: usize,
    /// Embedder used to convert text to vectors
    embedder: E,
}

impl<E: Debug + Embedder + Send + Sync> InMemoryVectorStore<E> {
    /// Creates a new empty vector store with the given embedder
    ///
    /// # Arguments
    /// * `embedder` - Implementation of the Embedder trait to use
    ///
    /// # Returns
    /// * A new InMemoryVectorStore instance
    pub fn new(embedder: E) -> Self {
        Self {
            documents: Vec::new(),
            next_id: 0,
            embedder,
        }
    }

    /// Calculates cosine similarity between two vectors
    ///
    /// # Arguments
    /// * `a` - First vector
    /// * `b` - Second vector
    ///
    /// # Returns
    /// * `f32` - Cosine similarity score between 0 and 1
    async fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm_a == 0.0 || norm_b == 0.0 {
            0.0
        } else {
            dot / (norm_a * norm_b)
        }
    }
}

#[async_trait]
impl<E: Debug + Embedder + Send + Sync> VectorStoreProvider for InMemoryVectorStore<E> {
    async fn add_document(&mut self, content: &str) -> Result<String, Box<dyn Error>> {
        let embedding = self.embedder.embed_text(content).await?;
        self.next_id += 1;
        let doc_id = format!("doc-{}", self.next_id);
        self.documents.push(DocumentEmbedding {
            id: doc_id.clone(),
            embedding,
            content: content.to_string(),
            metadata: None,
        });
        Ok(doc_id)
    }

    async fn search_documents(
        &self,
        query: &str,
        top_k: usize,
    ) -> Result<Vec<DocumentEmbedding>, Box<dyn Error>> {
        info!("Searching for documents with query: {}", query);
        info!("top_K: {:?}", top_k);
        let query_embedding = self.embedder.embed_text(query).await?;
        let mut scored: Vec<(f32, &DocumentEmbedding)> = Vec::new();
        for doc in &self.documents {
            let score = Self::cosine_similarity(&query_embedding, &doc.embedding).await;
            scored.push((score, doc));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        let results = scored
            .into_iter()
            .take(5)
            .map(|(_, doc)| (*doc).clone())
            .collect();
        Ok(results)
    }

    async fn upsert_document(
        &mut self,
        doc_id: &str,
        content: &str,
        metadata: Option<String>,
    ) -> Result<(), Box<dyn Error>> {
        let embedding = self.embedder.embed_text(content).await?;

        if let Some(pos) = self.documents.iter().position(|d| d.id == doc_id) {
            self.documents[pos].content = content.to_string();
            self.documents[pos].embedding = embedding;
            self.documents[pos].metadata = metadata;
        } else {
            self.documents.push(DocumentEmbedding {
                id: doc_id.to_string(),
                embedding,
                content: content.to_string(),
                metadata,
            });
        }
        Ok(())
    }
}
