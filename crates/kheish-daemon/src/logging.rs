use std::io::IsTerminal as _;
use std::sync::OnceLock;

use anyhow::Result;
use tracing::level_filters::LevelFilter;
use tracing_subscriber::EnvFilter;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum LogFormat {
    Pretty,
    Json,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
}

impl LogLevel {
    fn as_level_filter(self) -> LevelFilter {
        match self {
            Self::Error => LevelFilter::ERROR,
            Self::Warn => LevelFilter::WARN,
            Self::Info => LevelFilter::INFO,
            Self::Debug => LevelFilter::DEBUG,
            Self::Trace => LevelFilter::TRACE,
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) struct LoggingConfig {
    pub(crate) format: LogFormat,
    pub(crate) level: LogLevel,
}

static LOGGING_INITIALIZED: OnceLock<()> = OnceLock::new();

pub(crate) fn init_logging(config: LoggingConfig) -> Result<()> {
    if LOGGING_INITIALIZED.get().is_some() {
        return Ok(());
    }

    let filter = EnvFilter::try_from_default_env().or_else(|_| {
        EnvFilter::try_new(format!(
            "warn,kheish_agent={base},kheish_auth={base},kheish_core={base},kheish_coding_tools={base},kheish_daemon={base},kheish_mcp={base},kheish_output={base},kheish_runtime={base},kheish_session={base},kheish_skills={base},kheish_types={base},kheish.mcp.stderr={base},rmcp=off,hyper=warn,reqwest=warn,rustls_platform_verifier=warn",
            base = config.level.as_level_filter()
        ))
    })?;
    let timer = UtcTime::rfc_3339();

    let registry = tracing_subscriber::registry()
        .with(filter)
        .with(kheish_daemon::log_buffer::DaemonLogBufferLayer);
    let init_result = match config.format {
        LogFormat::Pretty => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_timer(timer)
                    .with_target(true)
                    .with_ansi(std::io::stderr().is_terminal())
                    .compact(),
            )
            .try_init(),
        LogFormat::Json => registry
            .with(
                tracing_subscriber::fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_timer(timer)
                    .with_target(true)
                    .with_ansi(false)
                    .json()
                    .flatten_event(true)
                    .with_current_span(false)
                    .with_span_list(true),
            )
            .try_init(),
    };
    match init_result {
        Ok(()) => {
            let _ = LOGGING_INITIALIZED.set(());
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}
