use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};

/// Module for interacting with the filesystem
pub struct FileSystemModule;

impl std::fmt::Debug for FileSystemModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileSystemModule")
    }
}

/// Splits text into chunks of roughly N characters.
///
/// # Arguments
/// * `content` - The text content to split
/// * `chunk_size` - Target size for each chunk in characters
///
/// # Returns
/// A vector of String chunks
fn chunk_text(content: &str, chunk_size: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let chars: Vec<char> = content.chars().collect();
    while start < chars.len() {
        let end = std::cmp::min(start + chunk_size, chars.len());
        let chunk: String = chars[start..end].iter().collect();
        chunks.push(chunk);
        start += chunk_size;
    }
    chunks
}

#[async_trait::async_trait]
impl Module for FileSystemModule {
    fn name(&self) -> &str {
        "fs"
    }

    /// Handles filesystem operations
    ///
    /// # Arguments
    /// * `vector_store` - The vector store for document storage
    /// * `action` - The action to perform (read, write, list_directory, etc)
    /// * `params` - Parameters for the action
    ///
    /// # Returns
    /// Result containing success message or error
    async fn handle_action(
        &self,
        vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String> {
        match action {
            "read" => {
                if params.is_empty() {
                    return Err("Missing parameter for 'read' action".into());
                }
                let path = &params[0];
                let full_path = path.to_string();
                let content = std::fs::read_to_string(&full_path).map_err(|e| e.to_string())?;

                let chunks = chunk_text(&content, 2000);
                for (i, c) in chunks.iter().enumerate() {
                    let doc_id = format!("{}#chunk-{}", full_path, i);
                    vector_store
                        .upsert_document(&doc_id, c, Some("code-chunk".into()))
                        .await
                        .map_err(|e| e.to_string())?;
                }

                Ok(format!(
                    "File '{}' indexed into RAG as {} chunks.",
                    full_path,
                    chunks.len()
                ))
            }

            "read_multiple" => {
                if params.is_empty() {
                    return Err("Missing parameter for 'read_multiple' action".into());
                }
                let paths = &params[0];
                let paths = paths.split(',').collect::<Vec<_>>();

                for p in paths {
                    let full_path = p.to_string();
                    let content = std::fs::read_to_string(&full_path).map_err(|e| e.to_string())?;
                    let chunks = chunk_text(&content, 2000);
                    for (i, c) in chunks.iter().enumerate() {
                        let doc_id = format!("{}#chunk-{}", full_path, i);
                        vector_store
                            .upsert_document(&doc_id, c, Some("code-chunk".into()))
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }

                Ok("All specified files indexed into RAG.".to_string())
            }

            "list_directory" => {
                if params.is_empty() {
                    return Err("Missing parameter for 'list_directory' action".into());
                }
                let path = &params[0];
                let entries = std::fs::read_dir(path).map_err(|e| e.to_string())?;
                let mut files = Vec::new();

                for entry in entries.flatten() {
                    if let Ok(file_name) = entry.file_name().into_string() {
                        let file_type = entry.file_type().map_err(|e| e.to_string())?;
                        let metadata = entry.metadata().map_err(|e| e.to_string())?;

                        let size = metadata.len();
                        let size_str = if size < 1024 {
                            format!("{}B", size)
                        } else if size < 1024 * 1024 {
                            format!("{:.1}KB", size as f64 / 1024.0)
                        } else {
                            format!("{:.1}MB", size as f64 / (1024.0 * 1024.0))
                        };

                        let entry_type = if file_type.is_dir() {
                            "[DIR]"
                        } else if file_type.is_symlink() {
                            "[LNK]"
                        } else {
                            "[FILE]"
                        };

                        files.push(format!("{:<6} {:<8} {}", entry_type, size_str, file_name));
                    }
                }

                files.sort();
                Ok(format!(
                    "Files found:\n{}",
                    files
                        .iter()
                        .enumerate()
                        .map(|(i, f)| format!("{}. {}", i + 1, f))
                        .collect::<Vec<_>>()
                        .join("\n")
                        + "\n"
                ))
            }

            "write" => {
                if params.len() < 2 {
                    return Err(
                        "Missing parameters for 'write' action (need path and content)".into(),
                    );
                }
                let path = &params[0];
                let content = &params[1];
                let full_path = path.to_string();
                std::fs::write(&full_path, content).map_err(|e| e.to_string())?;
                Ok("File written successfully".into())
            }

            _ => Err(format!("Unknown action '{}'", action)),
        }
    }

    /// Returns the list of available actions for this module
    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![
            ModuleAction {
                name: "read".to_string(),
                arg_count: 1,
                description: "Read a file and index it into RAG (no direct content return) usage: fs read <path>".to_string(),
            },
            ModuleAction {
                name: "list_directory".to_string(),
                arg_count: 1,
                description: "List files in a directory usage: fs list_directory <path>"
                    .to_string(),
            },
            ModuleAction {
                name: "write".to_string(),
                arg_count: 2,
                description: "Write to a file usage: fs write <path> <content>".to_string(),
            },
            ModuleAction {
                name: "read_multiple".to_string(),
                arg_count: 1,
                description: "Read multiple files and index into RAG usage: fs read_multiple <path1,path2,...>"
                    .to_string(),
            },
        ]
    }
}
