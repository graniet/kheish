use crate::config::AgentsConfig;
use crate::constants::*;
use crate::modules::ModulesManager;
use crate::llm::ChatMessage;

use tracing::debug;

/// Generates system instructions for the AI agents based on configuration and available modules
///
/// # Arguments
///
/// * `agent_config` - Configuration for different agent roles (proposer, reviewer, etc)
/// * `modules` - List of available modules that agents can use
///
/// # Returns
///
/// A string containing the complete system instructions including role-specific prompts
/// and module documentation if modules are available
pub fn generate_system_instructions(
    agent_config: &AgentsConfig,
    modules: &ModulesManager,
) -> String {
    let proposer_prompt = agent_config
        .proposer
        .system_prompt
        .clone()
        .unwrap_or(PROPOSER_SYSTEM_PROMPT.to_string());
    let reviewer_prompt = agent_config
        .reviewer
        .system_prompt
        .clone()
        .unwrap_or(REVIEWER_SYSTEM_PROMPT.to_string());
    let validator_prompt = agent_config
        .validator
        .system_prompt
        .clone()
        .unwrap_or(VALIDATOR_SYSTEM_PROMPT.to_string());
    let formatter_prompt = agent_config
        .formatter
        .system_prompt
        .clone()
        .unwrap_or(FORMATTER_SYSTEM_PROMPT.to_string());

    let mut system_instructions = String::from(
        "Global rules:\n\
    - Follow all instructions.\n\
    - When you play the 'proposer' role, follow these instructions:\n",
    );
    system_instructions.push_str(&proposer_prompt);
    system_instructions
        .push_str("\n\nWhen you play the 'reviewer' role, follow these instructions:\n");
    system_instructions.push_str(&reviewer_prompt);
    system_instructions
        .push_str("\n\nWhen you play the 'validator' role, follow these instructions:\n");
    system_instructions.push_str(&validator_prompt);
    system_instructions
        .push_str("\n\nWhen you play the 'formatter' role, follow these instructions:\n");
    system_instructions.push_str(&formatter_prompt);
    system_instructions.push_str("\n\nYou only have one system message (this one). Roles are activated by the user messages that follow.");

    if !modules.modules.is_empty() {
        system_instructions
            .push_str("\n\nYou have access to the following modules and their actions:\n");
        for m in modules.modules.iter() {
            let mod_name = m.name();
            system_instructions.push_str(&format!("Module '{}':\n", mod_name));
            let actions = m.get_actions();
            for act in actions {
                system_instructions.push_str(&format!(
                    "- {} ({} args): {}\n",
                    act.name, act.arg_count, act.description
                ));
            }
        }
        system_instructions.push_str("\nTo use a module, respond with:\nMODULE_REQUEST: <module_name> <action> <params...>\n");
        system_instructions
            .push_str("Use only the listed actions and the correct number of arguments.\n");
        system_instructions.push_str("Only one module request per response, if needed.\n");
    }

    system_instructions
}

/// Manages token count in a vector of messages by removing old messages or truncating
/// if the total token count exceeds the limit
///
/// # Arguments
/// * `messages` - Vector of ChatMessages to manage
/// * `token_limit` - Maximum number of tokens allowed
///
/// # Returns
/// * `bool` - True if messages were modified, False otherwise
pub fn manage_token_count(messages: &mut Vec<ChatMessage>, token_limit: usize) -> bool {
    let mut total_tokens: usize = messages.iter()
        .map(|msg| msg.content.chars().count())
        .sum();
        
    if total_tokens >= token_limit {
        if messages.len() > 1 {
            let mut i = 0;
            while total_tokens >= token_limit && i < messages.len() {
                if messages[i].role == "assistant" && messages[i].content.contains("proposal") {
                    total_tokens -= messages[i].content.chars().count();
                    messages.remove(i);
                    debug!("Removed proposal message to reduce token count. Remaining messages: {}", messages.len());
                } else {
                    i += 1;
                }
            }
        }
        
        if total_tokens >= token_limit && messages.len() > 1 {
            while total_tokens >= token_limit && messages.len() > 1 {
                if let Some(removed_msg) = messages.first() {
                    total_tokens -= removed_msg.content.chars().count();
                }
                messages.remove(0);
                debug!("Removed old message to reduce token count. Remaining messages: {}", messages.len());
            }
        }
        
        if total_tokens >= token_limit {
            if let Some(last_msg) = messages.last_mut() {
                last_msg.content = last_msg.content.chars()
                    .take(token_limit)
                    .collect::<String>();
                debug!("Truncated last message to fit token limit");
            }
        }
        return true;
    }
    false
}
