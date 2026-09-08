// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Error type shared by the core, the CLI and the Tauri commands.
//!
//! Every variant carries a stable `code()` for the window to translate.

use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("Network error: {0}")]
    Network(String),

    #[error("Feed could not be parsed: {0}")]
    FeedParse(String),

    #[error("Article extraction failed: {0}")]
    Extract(String),

    #[error("LLM model is not loaded")]
    ModelNotLoaded,

    #[error("Inference error: {0}")]
    Inference(String),

    #[error("Model download error: {0}")]
    ModelDownload(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Configuration error: {0}")]
    Config(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Other(String),

    /// A refusal the window has its own words for.
    ///
    /// The string is English, for the log and the command line; `code` is what
    /// the window looks up, and `arg` is the one value its sentence names.
    #[error("{message}")]
    Told { code: &'static str, message: String, arg: Option<String> },
}

impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            AppError::Network(_) => "network",
            AppError::FeedParse(_) => "feed_parse",
            AppError::Extract(_) => "extract",
            AppError::ModelNotLoaded => "model_not_loaded",
            AppError::Inference(_) => "inference",
            AppError::ModelDownload(_) => "model_download",
            AppError::Database(_) => "database",
            AppError::Config(_) => "config",
            AppError::Io(e) => io_code(e),
            AppError::Cancelled => "cancelled",
            AppError::Other(_) => "other",
            AppError::Told { code, .. } => code,
        }
    }

    pub fn detail(&self) -> Option<String> {
        match self {
            AppError::Network(s)
            | AppError::FeedParse(s)
            | AppError::Extract(s)
            | AppError::Inference(s)
            | AppError::ModelDownload(s)
            | AppError::Database(s)
            | AppError::Config(s)
            | AppError::Other(s) => Some(s.clone()),
            AppError::Io(e) => Some(e.to_string()),
            AppError::Told { arg, .. } => arg.clone(),
            AppError::ModelNotLoaded | AppError::Cancelled => None,
        }
    }

    /// A refusal the window will phrase itself. `message` is the English one,
    /// what the log and the command line get.
    pub fn told(code: &'static str, message: impl Into<String>) -> AppError {
        AppError::Told { code, message: message.into(), arg: None }
    }

    /// The same, for a sentence with something in it — the name of the model
    /// that was not found, an unrecognized source kind.
    pub fn told_about(
        code: &'static str,
        message: impl Into<String>,
        arg: impl Into<String>,
    ) -> AppError {
        AppError::Told { code, message: message.into(), arg: Some(arg.into()) }
    }

    /// True for a refusal the next article will run into as well: a rejected
    /// key or an empty account. The run stops rather than collecting the same
    /// answer once per article.
    pub fn is_settled(&self) -> bool {
        matches!(
            self.code(),
            "api_key_refused"
                | "api_no_such_model"
                | "api_out_of_credit"
                | "api_no_url"
                | "api_no_model"
        )
    }

    /// True for failures that are worth retrying on the next collection run
    /// (a flaky network) rather than recording as a permanent verdict.
    pub fn is_transient(&self) -> bool {
        matches!(self, AppError::Network(_) | AppError::Io(_))
    }
}

fn io_code(e: &std::io::Error) -> &'static str {
    use std::io::ErrorKind;
    match e.kind() {
        ErrorKind::NotFound => "file_not_found",
        ErrorKind::PermissionDenied => "permission_denied",
        _ => match e.raw_os_error() {
            Some(28) => "disk_full",  // ENOSPC
            Some(112) => "disk_full", // ERROR_DISK_FULL
            _ => "io",
        },
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(e: rusqlite::Error) -> Self {
        AppError::Database(e.to_string())
    }
}

impl From<reqwest::Error> for AppError {
    fn from(e: reqwest::Error) -> Self {
        AppError::Network(e.to_string())
    }
}

/// Serialized to the frontend as `{ code, message, detail }`.
#[derive(Serialize)]
struct ErrorPayload {
    code: &'static str,
    message: String,
    detail: Option<String>,
}

impl Serialize for AppError {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ErrorPayload { code: self.code(), message: self.to_string(), detail: self.detail() }
            .serialize(serializer)
    }
}

pub type AppResult<T> = Result<T, AppError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_settled_refusal_stops_the_run_and_a_passing_one_does_not() {
        // These will answer the same for the twenty-ninth article as for the
        // first, so the queue stops rather than paying to hear it again.
        for code in ["api_key_refused", "api_no_such_model", "api_out_of_credit"] {
            assert!(AppError::told(code, "x").is_settled(), "{code}");
        }

        // These might not. A busy service is busy now; a network drops and
        // comes back; one page can be unreadable while the next is fine.
        for e in [
            AppError::told("api_too_many", "x"),
            AppError::told("api_refused", "x"),
            AppError::Network("timeout".into()),
            AppError::Extract("no text".into()),
        ] {
            assert!(!e.is_settled(), "{}", e.code());
        }
    }

    #[test]
    fn a_named_refusal_carries_its_code_and_its_argument() {
        let e = AppError::told_about("unknown_model", "no model 'x'", "x");
        assert_eq!(e.code(), "unknown_model");
        assert_eq!(e.detail().as_deref(), Some("x"));
        assert_eq!(e.to_string(), "no model 'x'", "командной строке достаётся английский");
    }
}
