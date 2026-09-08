// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Shadow Tentacles: a local news aggregator.
//!
//! The core is headless; the CLI and the window are thin callers over it.

pub mod app;
pub mod cli;
pub mod config;
pub mod error;
pub mod llm;
pub mod models;
pub mod monitor_fix;
pub mod news;
pub mod paths;
pub mod pipeline;
pub mod shutdown;
pub mod store;
pub mod tts;

/// Sent on every outbound request. A real, identifiable agent string is the
/// minimum courtesy owed to the sites this reads, and it gives an operator
/// something to search for if it ever misbehaves.
pub const USER_AGENT: &str = concat!(
    "ShadowTentacles/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/shadow-tentacles)"
);
