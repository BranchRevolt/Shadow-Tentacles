// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Where the app keeps its files.
//!
//! Every location can be overridden with an environment variable. The models
//! directory is also a setting; `models_dir_or` is where the two meet.

use std::path::PathBuf;

use directories::ProjectDirs;

use crate::error::{AppError, AppResult};

const QUALIFIER: &str = "";
const ORGANIZATION: &str = "";
const APPLICATION: &str = "shadow-tentacles";

fn project_dirs() -> AppResult<ProjectDirs> {
    ProjectDirs::from(QUALIFIER, ORGANIZATION, APPLICATION)
        .ok_or_else(|| AppError::Config("could not determine the home directory".into()))
}

fn env_override(key: &str) -> Option<PathBuf> {
    std::env::var_os(key).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// Config directory. Holds `sources.toml` and `settings.toml`.
pub fn config_dir() -> AppResult<PathBuf> {
    if let Some(p) = env_override("SHADOW_CONFIG_DIR") {
        return Ok(p);
    }
    Ok(project_dirs()?.config_dir().to_path_buf())
}

/// Data directory. Holds the database and the cached audio.
pub fn data_dir() -> AppResult<PathBuf> {
    if let Some(p) = env_override("SHADOW_DATA_DIR") {
        return Ok(p);
    }
    Ok(project_dirs()?.data_dir().to_path_buf())
}

/// Where GGUF files go when nothing else is set.
pub fn models_dir() -> AppResult<PathBuf> {
    if let Some(p) = env_override("SHADOW_MODELS_DIR") {
        return Ok(p);
    }
    Ok(data_dir()?.join("models"))
}

/// Where this installation's GGUF files actually live.
///
/// `chosen` is the reader's setting, which wins because they picked it in the
/// window and a several-gigabyte file is the one thing on a small disk worth
/// moving. Blank means they have not chosen, and the default applies.
pub fn models_dir_or(chosen: Option<&str>) -> AppResult<PathBuf> {
    match chosen.map(str::trim).filter(|dir| !dir.is_empty()) {
        Some(dir) => Ok(PathBuf::from(dir)),
        None => models_dir(),
    }
}

/// Where voices and the pronunciation dictionaries live. Beside the models,
/// for the same reason: they are large, downloaded, and replaceable.
pub fn voices_dir() -> AppResult<PathBuf> {
    if let Some(p) = env_override("SHADOW_VOICES_DIR") {
        return Ok(p);
    }
    Ok(data_dir()?.join("voices"))
}

/// Where synthesised speech is kept. Safe to delete: everything in it can be
/// made again from the summary it came from.
pub fn audio_dir() -> AppResult<PathBuf> {
    Ok(data_dir()?.join("audio"))
}

pub fn database_path() -> AppResult<PathBuf> {
    Ok(data_dir()?.join("shadow.sqlite3"))
}

pub fn sources_path() -> AppResult<PathBuf> {
    Ok(config_dir()?.join("sources.toml"))
}

/// Create a directory and return it, so callers can chain.
pub fn ensure(dir: PathBuf) -> AppResult<PathBuf> {
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chosen_folder_wins_and_a_blank_one_does_not() {
        let chosen = models_dir_or(Some("/mnt/big/gguf")).unwrap();
        assert_eq!(chosen, PathBuf::from("/mnt/big/gguf"));

        // The settings file holds an empty string for "I have not chosen",
        // and a box someone typed spaces into means the same thing.
        for blank in [Some(""), Some("   "), None] {
            assert_eq!(
                models_dir_or(blank).unwrap(),
                models_dir().unwrap(),
                "пустой выбор — это отсутствие выбора, а не папка без имени"
            );
        }
    }
}
