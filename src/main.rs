// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Entry point: a subcommand runs headless, no subcommand opens the window.
//!
//! Exit runs through `teardown`, whatever caused it.

use std::process::ExitCode;
use std::time::Duration;

use shadow_tentacles::error::AppError;
use shadow_tentacles::shutdown::{self, GRACE_PERIOD, Shutdown};

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "shadow_tentacles=info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    if let Err(e) = shutdown::install_signal_handlers() {
        eprintln!("warning: could not install signal handlers: {e}");
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    // No subcommand opens the window; a subcommand runs headless. The same core
    // serves both, so neither is a lesser path.
    if args.is_empty() {
        shadow_tentacles::app::run();
        teardown();
        return ExitCode::SUCCESS;
    }

    let result = shadow_tentacles::cli::run(&args);

    teardown();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(AppError::Cancelled) => {
            eprintln!("cancelled");
            ExitCode::from(130)
        }
        Err(e) => {
            eprintln!("error [{}]: {e}", e.code());
            ExitCode::FAILURE
        }
    }
}

/// The single exit path.
///
/// Stop accepting work, let in-flight workers observe the flag and unwind, then
/// return from `main` so every `Drop` runs: the model's `Drop` is what releases
/// its memory, and `process::exit` would skip it.
fn teardown() {
    Shutdown::global().request();
    shutdown::arm_watchdog(GRACE_PERIOD + Duration::from_secs(2));
    if !Shutdown::global().wait_for_workers(GRACE_PERIOD) {
        tracing::warn!("exiting with work still in flight");
    }
}
