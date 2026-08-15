use std::{
    fs::{self, OpenOptions},
    path::Path,
};

use anyhow::Result;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, Registry, fmt, layer::SubscriberExt};

use crate::config::AppConfig;

const ASYNC_OPENAI_SAFE_FILTER: &str = "async_openai::client=error";

/// Initialize tracing with optional file logging.
/// Returns a WorkerGuard that must be held for the duration of the program
/// to ensure all logs are flushed to the file.
pub fn init_tracing(config: &AppConfig) -> Result<Option<WorkerGuard>> {
    let filter = EnvFilter::try_new(&config.log_level)
        .or_else(|_| EnvFilter::try_new("info"))?
        // async-openai logs the complete 5xx response body and 429 message at
        // WARN before returning the error. Those bodies may contain user or
        // provider data, so only this dependency target is raised to ERROR.
        .add_directive(ASYNC_OPENAI_SAFE_FILTER.parse()?);

    // If LOG_FILE_PATH is set, output to both stdout and file
    if let Some(ref log_path) = config.log_file_path {
        if let Some(parent) = Path::new(log_path).parent()
            && !parent.as_os_str().is_empty()
        {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;

        let (non_blocking, guard) = tracing_appender::non_blocking(file);

        let subscriber = Registry::default()
            .with(filter)
            // Stdout layer with colors
            .with(fmt::layer().with_target(true).with_line_number(true))
            // File layer without ANSI colors
            .with(
                fmt::layer()
                    .with_writer(non_blocking)
                    .with_ansi(false)
                    .with_target(true)
                    .with_line_number(true),
            );

        tracing::subscriber::set_global_default(subscriber)?;
        return Ok(Some(guard));
    }

    // Only stdout logging
    let subscriber = Registry::default()
        .with(filter)
        .with(fmt::layer().with_target(true).with_line_number(true));

    tracing::subscriber::set_global_default(subscriber)?;
    Ok(None)
}
