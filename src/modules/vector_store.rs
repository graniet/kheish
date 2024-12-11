use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};
use tracing::info;
pub struct VectorStoreModule;

impl std::fmt::Debug for VectorStoreModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "VectorStoreModule")
    }
}

#[async_trait::async_trait]
impl Module for VectorStoreModule {
    fn name(&self) -> &str {
        "rag"
    }

    async fn handle_action(&self, vector_store: &mut dyn VectorStoreProvider, action: &str, params: &[String]) -> Result<String, String> {
        match action {
            "search" => {
                let query = params.join(" ");
                let top_k = 5;
                let results = vector_store.search_documents(&query, top_k).await.map_err(|e| e.to_string())?;
                let mut content = vec![];
                for result in results {
                    content.push(format!("Search result: {}", result.content));
                }
                Ok(content.join("\n"))
            }
            "index" => {
                if params.is_empty() {
                    return Err("Missing parameter for 'index' action".into());
                }
                let path = &params[0];
                let full_path = path.to_string();
                let content = std::fs::read_to_string(&full_path).map_err(|e| e.to_string())?;
                let _ = vector_store.add_document(&content).await.map_err(|e| e.to_string())?;
                Ok(format!("File {} successfully, and added to vector store, use rag search to search for it", path))
            }
            "index_multiple" => {
                if params.is_empty() {
                    return Err("Missing parameter for 'read_multiple' action".into());
                }
                let paths = &params[0];
                let paths = paths.split(',').collect::<Vec<_>>();
                let mut files = Vec::new();
                let paths_str = paths.join(", ");
                for path in paths {
                    let full_path = path.to_string();
                    let content = std::fs::read_to_string(&full_path).map_err(|e| e.to_string())?;
                    files.push(format!("File: {}", path));
                    vector_store.add_document(&content).await.map_err(|e| e.to_string())?;
                }
                Ok(format!("Files {} successfully, and added to vector store, use rag search to search for it", paths_str))
            }
            _ => Err(format!("Unknown action '{}'", action)),
        }
    }

    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![ModuleAction {
            name: "search - Performs semantic search in the RAG vector store to find relevant documents. Usage: search <query text>. The query can be a question, keywords, or natural language text. Returns top 5 most semantically similar documents.".to_string(),
            arg_count: 1,
            description: "Search for similar documents".to_string(),
        }]
    }
}