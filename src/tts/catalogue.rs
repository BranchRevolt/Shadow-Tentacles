// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! The voices this program knows how to fetch.
//!
//! Curated rather than complete, and the sizes are the ones the server reports.

use crate::llm::Lang;

const BASE: &str = "https://huggingface.co/rhasspy/piper-voices/resolve/main";

#[derive(Debug, Clone, serde::Serialize)]
pub struct VoiceInfo {
    /// What the user picks and what the settings store: "ru_RU-irina-medium".
    pub id: String,
    /// A name to show, without the machinery in it.
    pub name: String,
    pub lang: Lang,
    /// Piper's own word for the tier: medium, high.
    pub quality: String,
    /// Size of the model file. The settings file beside it is a few kilobytes.
    pub size_bytes: u64,
    /// Where the two files live, without their extensions.
    pub base_url: String,
}

impl VoiceInfo {
    pub fn model_url(&self) -> String {
        format!("{}.onnx", self.base_url)
    }

    pub fn config_url(&self) -> String {
        format!("{}.onnx.json", self.base_url)
    }

    pub fn filename(&self) -> String {
        format!("{}.onnx", self.id)
    }
}

fn voice(lang: Lang, region: &str, speaker: &str, quality: &str, size: u64) -> VoiceInfo {
    let id = format!("{region}-{speaker}-{quality}");
    VoiceInfo {
        base_url: format!("{BASE}/{}/{region}/{speaker}/{quality}/{id}", lang.code()),
        name: pretty(speaker),
        quality: quality.to_string(),
        size_bytes: size,
        lang,
        id,
    }
}

/// A speaker's name as a person would write it.
fn pretty(speaker: &str) -> String {
    speaker
        .split('_')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Voices worth offering, by language.
pub fn catalog() -> Vec<VoiceInfo> {
    vec![
        // Russian: four, and no tier above medium exists.
        voice(Lang::Ru, "ru_RU", "irina", "medium", 63_201_294),
        voice(Lang::Ru, "ru_RU", "dmitri", "medium", 63_201_294),
        voice(Lang::Ru, "ru_RU", "ruslan", "medium", 63_201_294),
        voice(Lang::Ru, "ru_RU", "denis", "medium", 63_201_294),
        // English.
        voice(Lang::En, "en_GB", "cori", "high", 114_219_352),
        voice(Lang::En, "en_US", "lessac", "medium", 63_201_294),
        voice(Lang::En, "en_US", "amy", "medium", 63_201_294),
        voice(Lang::En, "en_GB", "alan", "medium", 63_201_294),
        // German.
        voice(Lang::De, "de_DE", "thorsten", "high", 113_895_201),
        voice(Lang::De, "de_DE", "thorsten", "medium", 63_201_294),
        voice(Lang::De, "de_DE", "mls", "medium", 76_961_079),
        // French.
        voice(Lang::Fr, "fr_FR", "siwis", "medium", 63_201_294),
        voice(Lang::Fr, "fr_FR", "tom", "medium", 63_511_038),
        voice(Lang::Fr, "fr_FR", "upmc", "medium", 76_733_615),
        // Spanish, from both sides of the Atlantic. Which espeak dialect each
        // one wants is in its own config, so a Mexican voice is not read with a
        // Castilian accent — see `phonemes`.
        voice(Lang::Es, "es_ES", "davefx", "medium", 63_201_294),
        voice(Lang::Es, "es_ES", "sharvard", "medium", 76_733_615),
        voice(Lang::Es, "es_MX", "ald", "medium", 63_201_294),
        voice(Lang::Es, "es_AR", "daniela", "high", 114_199_011),
        // Portuguese: the published voices are Brazilian, and this is the only
        // tier above x_low that exists for them.
        voice(Lang::Pt, "pt_BR", "faber", "medium", 63_201_294),
        voice(Lang::Pt, "pt_BR", "cadu", "medium", 62_950_044),
        voice(Lang::Pt, "pt_BR", "jeff", "medium", 62_950_044),
    ]
}

pub fn by_id(id: &str) -> Option<VoiceInfo> {
    catalog().into_iter().find(|v| v.id == id)
}

pub fn for_lang(lang: Lang) -> Vec<VoiceInfo> {
    catalog().into_iter().filter(|v| v.lang == lang).collect()
}

/// Is this voice on disk? Both files, since one without the other cannot load.
pub fn is_installed(dir: &std::path::Path, id: &str) -> bool {
    dir.join(format!("{id}.onnx")).is_file() && dir.join(format!("{id}.onnx.json")).is_file()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_something_to_offer() {
        for lang in Lang::ALL {
            assert!(!for_lang(lang).is_empty(), "нет ни одного голоса для {lang}");
        }
    }

    #[test]
    fn the_urls_are_built_the_way_the_repository_lays_them_out() {
        let irina = by_id("ru_RU-irina-medium").expect("этот голос есть в каталоге");
        assert_eq!(
            irina.model_url(),
            "https://huggingface.co/rhasspy/piper-voices/resolve/main/ru/ru_RU/irina/medium/ru_RU-irina-medium.onnx"
        );
        // The settings file sits beside the model under the same name.
        assert_eq!(irina.config_url(), format!("{}.json", irina.model_url()));
    }

    #[test]
    fn identifiers_are_unique() {
        let mut ids: Vec<String> = catalog().into_iter().map(|v| v.id).collect();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before, "в каталоге повторяются идентификаторы");
    }

    #[test]
    fn a_speaker_name_is_shown_readably() {
        assert_eq!(pretty("irina"), "Irina");
        assert_eq!(pretty("jenny_dioco"), "Jenny Dioco");
    }

    #[test]
    fn russian_has_no_tier_above_medium() {
        // Not a gap in this list: it is what the project publishes, and the
        // settings screen should not imply otherwise.
        assert!(for_lang(Lang::Ru).iter().all(|v| v.quality == "medium"));
        assert!(for_lang(Lang::En).iter().any(|v| v.quality == "high"));
    }
}
