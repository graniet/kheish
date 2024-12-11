/// Represents a chat message with a role and content
#[derive(serde::Serialize, Debug, Clone)]
pub struct ChatMessage {
    /// Role of the message sender (e.g. "system", "user", "assistant")
    pub role: String,
    /// Content/text of the message
    pub content: String,
}

impl ChatMessage {
    /// Creates a new chat message
    ///
    /// # Arguments
    /// * `role` - Role of the message sender
    /// * `content` - Content/text of the message
    ///
    /// # Returns
    /// * `ChatMessage` - New chat message instance
    pub fn new(role: &str, content: &str) -> Self {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
        }
    }
}
