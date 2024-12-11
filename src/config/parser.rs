use super::TaskConfig;
use std::error::Error;
use std::fs;

use tracing::info;

/// Loads and parses a task configuration from a YAML file
///
/// # Arguments
///
/// * `file_path` - Path to the YAML configuration file
///
/// # Returns
///
/// * `Result<TaskConfig, Box<dyn Error>>` - The parsed TaskConfig on success, or an error if loading/parsing fails
///
/// # Errors
///
/// Returns an error if:
/// * The file cannot be read
/// * The YAML content cannot be parsed into a TaskConfig
pub fn load_task_config(file_path: &str) -> Result<TaskConfig, Box<dyn Error>> {
    let yaml_str = fs::read_to_string(file_path)?;
    let task_config: TaskConfig = serde_yaml::from_str(&yaml_str)?;
    info!("Loaded task configuration: {:?}", task_config.name);
    Ok(task_config)
}
