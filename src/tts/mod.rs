// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading the news aloud.
//!
//! Two engines behind one call: the local one runs on this machine, the online
//! one sends the text to Microsoft. See `Engine`.

pub mod catalogue;
pub mod espeak_data;
pub mod normalize;
pub mod online;
pub mod phonemes;
pub mod player;
pub mod stress;
pub mod voice;

use serde::{Deserialize, Serialize};

use crate::error::AppResult;
use crate::llm::Lang;

pub use player::Player;
pub use voice::Voice;

/// Where the speech is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    /// On this machine, with Piper. Free, private, and as good as offline
    /// Russian currently gets — which is one voice worth listening to.
    #[default]
    Local,
    /// Microsoft's reading service. Close to human, and it means the text of
    /// the summary leaves the machine. Never the default, and never selected
    /// without the user being told what it costs.
    Online,
}

impl Engine {
    pub fn as_str(self) -> &'static str {
        match self {
            Engine::Local => "local",
            Engine::Online => "online",
        }
    }

    /// What using this engine costs, for the log and the command line.
    ///
    /// The window says this in its own seven languages, keyed off the engine
    /// name — a sentence about privacy is the last one to hand a reader in a
    /// language they do not read.
    pub fn caveat(self) -> Option<&'static str> {
        match self {
            Engine::Local => None,
            Engine::Online => {
                Some("needs a connection; the text of the summary is sent to Microsoft")
            }
        }
    }
}

impl std::str::FromStr for Engine {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "piper" | "offline" => Ok(Engine::Local),
            "online" | "edge" | "msedge" => Ok(Engine::Online),
            other => Err(format!("unknown speech engine '{other}' (expected local or online)")),
        }
    }
}

/// Read `text` aloud with whichever engine was chosen.
///
/// The local engine needs a loaded `Voice`; the online one needs nothing but a
/// connection, which is why it takes the voice by name rather than by handle.
pub fn speak(
    engine: Engine,
    local: &Voice,
    text: &str,
    lang: Lang,
    voice_name: Option<&str>,
    rate: f32,
) -> AppResult<(Vec<f32>, u32)> {
    match engine {
        Engine::Local => local.speak(text, lang, rate),
        Engine::Online => {
            // Normalization is shared: numbers and abbreviations are read
            // badly by both engines for the same reasons, and the online one
            // takes text rather than phonemes, so this is where it applies.
            let prepared = normalize::normalize(text, lang);
            tracing::debug!("online: {} chars after normalizing", prepared.chars().count());
            online::speak(&prepared, lang, voice_name, rate)
        }
    }
}
