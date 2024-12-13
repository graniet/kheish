use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::fmt;
use tracing_subscriber::prelude::*;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Initialize the logging system with the specified log level.
/// Logs are written to daily rotating files in the "logs" directory.
///
/// # Arguments
///
/// * `log_level` - The desired log level as a string (e.g. "info", "debug", "warn")
/// * `with_file` - Whether to also log to a rotating file in addition to stdout.
///
/// # Example
///
/// ```
/// init_logging("info");
/// ```
pub fn init_logging(log_level: &str) {
    let filter = match EnvFilter::try_new(log_level) {
        Ok(f) => f,
        Err(_) => {
            eprintln!("Invalid log level '{}', defaulting to 'info'", log_level);
            EnvFilter::new("info")
        }
    };

    let file_appender = RollingFileAppender::new(Rotation::DAILY, "logs", "kheish.log");

    let file_layer = fmt::layer()
        .with_line_number(true)
        .with_writer(file_appender);

    tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .init();
}
