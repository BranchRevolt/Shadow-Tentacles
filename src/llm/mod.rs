// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Local inference: the resident llama.cpp engine and everything built on it.

pub mod chunker;
pub mod engine;
pub mod lang;
pub mod remote;
pub mod summarize;

pub use engine::{Engine, GenOptions};
pub use lang::Lang;
pub use summarize::{ArticleSummary, Mode, Outcome, prompt_version, summarize_article};
