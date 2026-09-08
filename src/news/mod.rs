// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Collecting the news: sources, fetching, extraction and the gates that decide
//! what is worth spending inference on.

pub mod dedup;
pub mod extract;
pub mod fetch;
pub mod filters;
pub mod quality;
pub mod sources;
pub mod types;

pub use fetch::Fetcher;
pub use types::{Article, Candidate, Metrics, Status};
