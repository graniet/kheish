use crate::core::rag::VectorStoreProvider;
use crate::modules::{Module, ModuleAction};
use std::process::Command;
use tracing::{debug, info};

/// Module for executing shell commands with configurable restrictions
#[derive(Debug)]
pub struct ShModule {
    /// List of allowed shell commands, empty means all commands allowed
    pub allowed_commands: Vec<String>,
}

impl ShModule {
    /// Creates a new ShModule with the specified allowed commands
    ///
    /// # Arguments
    /// * `allowed_commands` - List of shell commands that are allowed to be executed
    ///
    /// # Returns
    /// * `ShModule` - New shell module instance
    pub fn new(allowed_commands: Vec<String>) -> Self {
        ShModule { allowed_commands }
    }

    /// Checks if a command is allowed to be executed
    ///
    /// # Arguments
    /// * `cmd` - Command to check
    ///
    /// # Returns
    /// * `bool` - True if command is allowed, false otherwise
    fn is_allowed(&self, cmd: &str) -> bool {
        self.allowed_commands.is_empty() || self.allowed_commands.contains(&cmd.to_string())
    }
}

#[async_trait::async_trait]
impl Module for ShModule {
    /// Returns the name of this module
    fn name(&self) -> &str {
        "sh"
    }

    /// Handles shell command execution
    ///
    /// # Arguments
    /// * `_vector_store` - Vector store provider (unused)
    /// * `action` - Action to perform ("run")
    /// * `params` - Command and arguments to execute
    ///
    /// # Returns
    /// * `Result<String, String>` - Command output or error message
    async fn handle_action(
        &self,
        _vector_store: &mut dyn VectorStoreProvider,
        action: &str,
        params: &[String],
    ) -> Result<String, String> {
        match action {
            "run" => {
                if params.is_empty() {
                    return Err("Missing command to run".into());
                }

                let command = &params[0];
                let args = &params[1..];

                if !self.is_allowed(command) {
                    return Err(format!("Command '{}' not allowed", command));
                }

                debug!("Running command: {} {:?}", command, args);

                let output = Command::new(command)
                    .args(args)
                    .output()
                    .map_err(|e| e.to_string())?;

                let stdout = String::from_utf8_lossy(&output.stdout).to_string();
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();

                if !stderr.trim().is_empty() {
                    info!("STDOUT:\n{}\n\nSTDERR:\n{}", stdout, stderr);
                    Ok(format!("STDOUT:\n{}\n\nSTDERR:\n{}", stdout, stderr))
                } else {
                    Ok(stdout)
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
        vec![ModuleAction {
            name: "run".into(),
            arg_count: 1,
            description: format!(
                "Run a shell command. Allowed commands: {}. Usage: run <command> [args...]",
                if self.allowed_commands.is_empty() {
                    "all".to_string()
                } else {
                    self.allowed_commands.join(", ")
                }
            ),
        }]
    }
}
