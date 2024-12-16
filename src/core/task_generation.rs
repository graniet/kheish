use crate::config::TaskConfig;
use crate::constants::SYSTEM_PROMPT_TASK_CONFIG;
use crate::llm::{ChatMessage, LlmClient};
use chrono::Local;
use colored::*;
use dialoguer::{theme::ColorfulTheme, Input};
use serde_yaml;
use std::fs;
use std::path::Path;

/// Base template for task configuration loaded from file
static BASE_TEMPLATE: &str = include_str!("../templates/base_task.yaml");

/// Separator line used for visual formatting
const SEPARATOR: &str = "\n━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━\n";

/// ANSI escape sequence to clear the terminal screen
const CLEAR_SCREEN: &str = "\x1B[2J\x1B[1;1H";

/// Displays a formatted header with the given title
///
/// Clears the screen and prints the title between separator lines
///
/// # Arguments
/// * `title` - The title text to display in the header
fn display_header(title: &str) {
    print!("{}", CLEAR_SCREEN);
    println!("{}{}{}", SEPARATOR, title.blue().bold(), SEPARATOR);
}

/// Saves the task configuration YAML to a timestamped file
///
/// Creates a tasks directory if it doesn't exist and saves the YAML content
/// to a file with format "tasks/generated_task_YYYYMMDD_HHMMSS.yaml"
///
/// # Arguments
/// * `yaml_content` - The YAML configuration content to save
///
/// # Returns
/// * `Result<String, std::io::Error>` - The filename on success, or error on failure
fn save_task_config(yaml_content: &str) -> Result<String, std::io::Error> {
    let timestamp = Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("tasks/generated_task_{}.yaml", timestamp);

    if !Path::new("tasks").exists() {
        fs::create_dir_all("tasks")?;
    }

    fs::write(&filename, yaml_content)?;
    Ok(filename)
}

/// Generates a task configuration based on user input and LLM interaction
///
/// Takes a user request and interacts with an LLM to generate a valid task configuration.
/// Handles additional information requests and validates the generated YAML.
///
/// # Arguments
/// * `user_request` - The initial user request for task generation
/// * `llm_client` - The LLM client for API interaction
///
/// # Returns
/// * `TaskConfig` - The validated task configuration
pub async fn generate_task_config_from_user(
    user_request: &str,
    llm_client: &LlmClient,
) -> TaskConfig {
    display_header("🔧 Task Generation");

    let mut messages = vec![
        ChatMessage::new("system", SYSTEM_PROMPT_TASK_CONFIG),
        ChatMessage::new(
            "user",
            format!("Here is the base template:\n```\n{}\n```", BASE_TEMPLATE).as_str(),
        ),
        ChatMessage::new("user", format!("Your request:\n{}", user_request).as_str()),
    ];

    loop {
        println!("{}", "⏳ Generating configuration...".cyan().italic());

        let mut response = llm_client
            .call_llm_api(messages.clone())
            .await
            .expect("LLM API call failed");

        messages.push(ChatMessage::new("assistant", &response));

        if response.to_lowercase().starts_with("need_info:") {
            display_header("❓ Additional Information Required");
            response = response.replace("NEED_INFO:", "");
            println!("{}\n", response.cyan());

            let user_input: String = Input::with_theme(&ColorfulTheme::default())
                .with_prompt("Your response")
                .interact_text()
                .expect("Failed to read input");

            messages.push(ChatMessage::new("user", user_input.trim()));
            println!("{}", "⏳ Processing...".green().italic());
            continue;
        }

        if let Some((yaml_content, is_valid)) = extract_and_validate_yaml(&response) {
            if is_valid {
                match serde_yaml::from_str::<TaskConfig>(&yaml_content) {
                    Ok(config) => {
                        display_header("✅ Configuration Validated");

                        match save_task_config(&yaml_content) {
                            Ok(filename) => println!(
                                "{} Configuration saved to {}",
                                "✓".green(),
                                filename.bold()
                            ),
                            Err(e) => println!("{} Error saving configuration: {}", "✗".red(), e),
                        }

                        println!("{}", SEPARATOR);
                        return config;
                    }
                    Err(e) => {
                        println!(
                            "\n{} YAML validation error:\n{}",
                            "⚠️".red().bold(),
                            e.to_string().red()
                        );
                        messages.push(ChatMessage::new(
                            "user",
                            "The YAML is invalid. Please provide a valid configuration.",
                        ));
                        continue;
                    }
                }
            }
        }

        println!("{} Invalid format, retrying...", "⚠️".yellow());
        messages.push(ChatMessage::new(
            "user",
            "Please provide the YAML in a proper ```yaml ... ``` code block.",
        ));
    }
}

/// Extracts and validates YAML content from an LLM response
///
/// Looks for YAML content between ```yaml and ``` markers, cleans it up,
/// and returns the content with a validation flag.
///
/// # Arguments
/// * `response` - The full response string from the LLM
///
/// # Returns
/// * `Option<(String, bool)>` - Tuple of (YAML content, is_valid) or None if no YAML found
fn extract_and_validate_yaml(response: &str) -> Option<(String, bool)> {
    let yaml_start = response.find("```")?;
    let yaml_end = response[yaml_start + 3..].find("```")?;

    let yaml_content = response[yaml_start + 3..yaml_start + 3 + yaml_end]
        .trim()
        .replace("yaml", "")
        .trim()
        .to_string();

    Some((yaml_content, true))
}
