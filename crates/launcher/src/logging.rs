// SPDX-License-Identifier: GPL-3.0-or-later
//! Persistent file logging for the launcher.
//!
//! The tray build has no console and the headless build is often left running
//! for hours, so stderr-only tracing loses exactly the events needed to
//! diagnose a wedge (e.g. the 2026-09-16 rig run where the MCP server went
//! unresponsive ~1h after the sim exited). `init` therefore fans out to both
//! stderr and a daily-rotated file under `<config dir>/logs/`; `RUST_LOG`
//! still controls filtering, defaulting to `info` so sampler debug chatter
//! stays out of the file.

use std::fs;
use std::panic;
use std::path::PathBuf;

use tracing::info;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

/// Guard keeping the non-blocking file writer alive; must live as long as
/// the process or buffered lines are dropped.
pub struct LogGuard {
    _guard: tracing_appender::non_blocking::WorkerGuard,
}

/// The directory the rotated log files land in.
pub fn log_dir() -> PathBuf {
    crate::config::config_dir().join("logs")
}

/// Install the global subscriber (stderr + rotated file) and a panic hook
/// that records panics into the same stream before unwinding.
pub fn init() -> Result<LogGuard, String> {
    let dir = log_dir();
    fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create {}: {error}", dir.display()))?;
    let file_appender = tracing_appender::rolling::daily(&dir, "launcher.log");
    let (writer, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let stderr_layer = fmt::layer().with_writer(std::io::stderr);
    let file_layer = fmt::layer().with_ansi(false).with_writer(writer);

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .try_init()
        .map_err(|error| error.to_string())?;

    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |panic_info| {
        tracing::error!(%panic_info, "launcher thread panicked");
        previous(panic_info);
    }));

    info!(dir = %dir.display(), "launcher file logging enabled");
    Ok(LogGuard { _guard: guard })
}
