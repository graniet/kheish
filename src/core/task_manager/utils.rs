use indicatif::ProgressBar;
use tokio::time::{sleep, Duration};

/// Updates the spinner message and pauses execution for 2 seconds
///
/// # Arguments
/// * `spinner` - Progress bar instance to update
/// * `message` - New message to display
pub async fn pause_and_update(spinner: &ProgressBar, message: &str) {
    spinner.set_message(message.to_string());
    sleep(Duration::from_secs(2)).await;
}
