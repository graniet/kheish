use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};

/// Module for managing a "long-term memory" using the vector store.
/// This memory is conceptual and not linked to files. The agent can insert arbitrary text (summaries, notes)
/// and later recall it by semantic search.
pub struct MemoriesModule;

impl std::fmt::Debug for MemoriesModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MemoriesModule")
    }
}

#[async_trait::async_trait]
impl Module for MemoriesModule {
    /// Returns the name of this module
    fn name(&self) -> &str {
        "memories"
    }

    /// Handles memory operations for storing and retrieving information
    ///
    /// # Arguments
    /// * `vector_store` - Vector store provider for storing embeddings
    /// * `action` - Action to perform ("insert" or "recall")
    /// * `params` - Text content or search query
    ///
    /// # Returns
    /// * `Result<String, String>` - Success message or error
    async fn handle_action(
        &self,
        vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String> {
        match action {
            "insert" => {
                if params.is_empty() {
                    return Err("Missing content for 'insert' action".into());
                }
                let content = params.join(" ");
                vector_store
                    .add_document_with_id("mem", &content)
                    .await
                    .map_err(|e| e.to_string())?;
                Ok("Content successfully inserted into memories (tagged with 'mem'). You can recall it later with 'memories recall <query>'.".to_string())
            }
            "recall" => {
                if params.is_empty() {
                    return Err("Missing query for 'recall' action".into());
                }
                let query = params.join(" ");
                let top_k = 5;
                let results = vector_store
                    .search_documents(&query, top_k)
                    .await
                    .map_err(|e| e.to_string())?;

                let mem_results = results
                    .into_iter()
                    .filter(|r| r.id.contains("mem"))
                    .collect::<Vec<_>>();

                if mem_results.is_empty() {
                    Ok("No relevant memories found.".to_string())
                } else {
                    let mut content = vec![];
                    for (i, result) in mem_results.iter().enumerate() {
                        let cleaned = result
                            .content
                            .lines()
                            .skip_while(|line| line.starts_with("MEMORY_TAG: "))
                            .collect::<Vec<_>>()
                            .join("\n");
                        content.push(format!("{}: {}", i + 1, cleaned));
                    }
                    Ok(format!("Memories found:\n{}", content.join("\n")))
                }
            }
            _ => Err(format!("Unknown action '{}'", action)),
        }
    }

    /// Returns the list of available actions for this module
    ///
    /// # Returns
    /// * `Vec<ModuleAction>` - List of available actions and their descriptions
    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![
            ModuleAction {
                name: "insert".to_string(),
                arg_count: 1,
                description: "Insert a piece of text into the memories. Usage: insert <text>"
                    .to_string(),
            },
            ModuleAction {
                name: "recall".to_string(),
                arg_count: 1,
                description:
                    "Recall information from memories by semantic search. Usage: recall <query>"
                        .to_string(),
            },
        ]
    }
}
