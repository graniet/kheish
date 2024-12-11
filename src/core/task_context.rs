/// Represents the context for a task, containing both file-based and text-based content
#[derive(Debug, Clone)]
pub struct TaskContext {
    /// Vector of tuples containing file names and their content
    pub files: Vec<(String, String)>,
    /// Raw text content for the task
    pub text: String,
}

impl TaskContext {
    /// Creates a new empty TaskContext
    ///
    /// # Returns
    ///
    /// A new TaskContext instance with empty files and text
    pub fn new() -> Self {
        Self {
            files: vec![],
            text: String::new(),
        }
    }

    /// Combines all context sources into a single formatted string
    ///
    /// Merges both text content and file contents into a formatted string,
    /// with clear section headers for each type of content.
    ///
    /// # Returns
    ///
    /// A String containing the combined context, or empty string if no content exists
    pub fn combined_context(&self) -> String {
        if self.text.trim().is_empty() && self.files.is_empty() {
            return String::new();
        }

        let mut combined = String::with_capacity(
            self.text.len()
                + self
                    .files
                    .iter()
                    .map(|(a, c)| a.len() + c.len() + 20)
                    .sum::<usize>(),
        );

        if !self.text.trim().is_empty() {
            combined.push_str("Text context:\n");
            combined.push_str(&self.text);
            combined.push_str("\n\n");
        }

        if !self.files.is_empty() {
            combined.push_str("Files context:\n");
            for (alias, content) in &self.files {
                combined.push_str("File '");
                combined.push_str(alias);
                combined.push_str("':\n");
                combined.push_str(content);
                combined.push_str("\n\n");
            }
        }

        combined
    }
}
