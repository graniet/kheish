use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};

/// Module for interacting with the filesystem
pub struct FileSystemModule;

impl std::fmt::Debug for FileSystemModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "FileSystemModule")
    }
}

#[async_trait::async_trait]
impl Module for FileSystemModule {
    /// Returns the name of this module
    fn name(&self) -> &str {
        "fs"
    }

    /// Handles filesystem actions like reading and writing files
    ///
    /// # Arguments
    /// * `_vector_store` - Vector store provider (unused)
    /// * `action` - Action to perform ("read", "write", etc)
    /// * `params` - Parameters for the action
    ///
    /// # Returns
    /// * `Result<String, String>` - Success message or error
    async fn handle_action(
        &self,
        _vector_store: &mut dyn VectorStoreProvider,
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
                std::fs::read_to_string(&full_path).map_err(|e| e.to_string())
            }
            "read_multiple" => {
                if params.is_empty() {
                    return Err("Missing parameter for 'read_multiple' action".into());
                }
                let paths = &params[0];
                let paths = paths.split(',').collect::<Vec<_>>();
                let mut files = Vec::new();
                for path in paths {
                    let full_path = path.to_string();
                    let content = std::fs::read_to_string(&full_path).map_err(|e| e.to_string())?;
                    files.push(format!("File: {}\n{}", path, content));
                }
                Ok(files.join("\n\n"))
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
                        files.push(file_name);
                    }
                }
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
    ///
    /// # Returns
    /// * `Vec<ModuleAction>` - List of supported actions and their descriptions
    fn get_actions(&self) -> Vec<ModuleAction> {
        vec![
            ModuleAction {
                name: "read".to_string(),
                arg_count: 1,
                description: "Read a file usage: fs read <path>".to_string(),
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
                description: "Read multiple files usage: fs read_multiple <path1,path2,...>"
                    .to_string(),
            },
        ]
    }
}
