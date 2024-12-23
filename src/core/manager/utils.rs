use crate::core::TaskManager;
use chrono::{Duration as ChronoDuration, NaiveDateTime, Utc};
use indicatif::ProgressBar;
use indicatif::ProgressStyle;
use tracing::info;

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
    info!("{}", message);
    spinner.set_message(message.to_string());
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
        if self.without_task {
            return;
        }

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

/// Determines if a task should be executed based on its interval and last run time
///
/// # Arguments
///
/// * `interval_str` - A string representing the interval duration (e.g. "1h", "30m")
/// * `last_run_at` - The timestamp when the task was last executed
///
/// # Returns
///
/// Returns `true` if enough time has elapsed since the last run based on the interval,
/// `false` otherwise or if there are any parsing errors
///
/// # Examples
///
/// ```
/// use chrono::NaiveDateTime;
/// let last_run = NaiveDateTime::from_timestamp_opt(0, 0).unwrap();
/// let should_run = check_should_execute_now("1h", last_run);
/// ```
pub fn check_should_execute_now(interval_str: &str, last_run_at: NaiveDateTime) -> bool {
    match humantime::parse_duration(interval_str) {
        Ok(std_dur) => match ChronoDuration::from_std(std_dur) {
            Ok(chrono_dur) => {
                let next_run_at = last_run_at + chrono_dur;
                let now = Utc::now().naive_utc();
                next_run_at <= now
            }
            Err(_) => false,
        },
        Err(_) => false,
    }
}
