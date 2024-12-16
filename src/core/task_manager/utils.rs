use crate::core::TaskManager;
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use tokio::time::{sleep, Duration};

/// Updates the spinner message and pauses execution for 500ms
///
/// This function updates the message displayed by the progress spinner and introduces
/// a 500ms delay in execution.
///
/// # Arguments
///
/// * `spinner` - The progress bar instance to update with the new message
/// * `message` - The new message string to display in the spinner
///
/// # Examples
///
/// ```
/// let spinner = ProgressBar::new(100);
/// pause_and_update(&spinner, "Processing...").await;
/// ```
pub async fn pause_and_update(spinner: &ProgressBar, message: &str) {
    spinner.set_message(message.to_string());
    sleep(Duration::from_millis(500)).await;
}

impl TaskManager {
    /// Initializes the progress spinner with default styling
    ///
    /// Sets up the spinner with:
    /// - 120ms tick rate for smooth animation
    /// - Unicode spinner characters for visual feedback
    /// - Template showing spinner, elapsed time and message
    ///
    /// # Examples
    ///
    /// ```
    /// let mut task_manager = TaskManager::new();
    /// task_manager.init_spinner();
    /// ```
    pub fn init_spinner(&mut self) {
        self.spinner
            .enable_steady_tick(std::time::Duration::from_millis(120));
        self.spinner.set_style(
            ProgressStyle::default_spinner()
                .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏")
                .template("{spinner} [{elapsed_precise}] {msg}")
                .expect("Failed to set spinner template"),
        );
    }
}
