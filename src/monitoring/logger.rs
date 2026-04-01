use anyhow::Result;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_subscriber::prelude::*;

/// Initialize structured logging for RICOZ SNIPER.
///
/// Reads the `RUST_LOG` environment variable to configure log levels.
/// Defaults to `info` level if `RUST_LOG` is not set.
///
/// Logs are written to stdout with timestamps and target module names.
/// If `log_file` is provided, logs are also written to the specified file.
pub fn init_logging(log_file: Option<&str>) -> Result<()> {
    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,ricoz_sniper=debug"));

    let stdout_layer = fmt::layer()
        .with_target(true)
        .with_thread_ids(false)
        .with_thread_names(false)
        .with_ansi(true);

    if let Some(path) = log_file {
        // Ensure the parent directory exists.
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;

        let file_layer = fmt::layer()
            .with_target(true)
            .with_ansi(false)
            .with_writer(file);

        tracing_subscriber::registry()
            .with(env_filter)
            .with(stdout_layer)
            .with(file_layer)
            .init();
    } else {
        tracing_subscriber::registry()
            .with(env_filter)
            .with(stdout_layer)
            .init();
    }

    tracing::info!("RICOZ SNIPER logging initialized");
    Ok(())
}
