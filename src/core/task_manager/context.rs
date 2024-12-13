use crate::config::TaskConfig;
use crate::core::task_context::TaskContext;
use std::fs;
use std::io::{self, Write};
use tracing::error;

/// Processes the task context configuration to build a TaskContext object
///
/// Takes a TaskConfig and processes each context item based on its kind:
/// - "file": Reads content from specified file path
/// - "user_input": Gets input from user or uses provided content
/// - "text": Uses provided text content directly
///
/// # Arguments
/// * `config` - Reference to TaskConfig containing context configuration
///
/// # Returns
/// * `TaskContext` - Constructed task context with processed content
pub fn process_task_context(config: &TaskConfig) -> TaskContext {
    let mut ctx = TaskContext::new();

    for item in &config.context {
        match item.kind.as_str() {
            "file" => {
                if let Some(path) = &item.path {
                    let content = fs::read_to_string(path)
                        .unwrap_or_else(|_| panic!("Failed to read file: {}", path));
                    let alias = item.alias.clone().unwrap_or_else(|| path.clone());
                    ctx.files.push((alias, content));
                }
            }
            "user_input" => {
                if let Some(content) = &item.content {
                    ctx.text.push_str(content);
                    ctx.text.push('\n');
                } else {
                    print!("Input |> ");
                    io::stdout().flush().expect("Failed to flush stdout");
                    let mut input = String::new();
                    io::stdin()
                        .read_line(&mut input)
                        .expect("Failed to read user input");
                    ctx.text.push_str(&input);
                }
            }
            "text" => {
                if let Some(content) = &item.content {
                    ctx.text.push_str(content);
                    ctx.text.push('\n');
                }
            }
            kind => {
                error!("Unknown context kind: {}", kind);
            }
        }
    }

    ctx
}
