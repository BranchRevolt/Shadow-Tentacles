// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! The languages a summary can be written in.
//!
//! Fewer than the interface speaks: each needs a stemmer, a detector mapping,
//! quality-gate phrases, number handling and a voice.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Ru,
    En,
    De,
    Fr,
    Es,
    /// Portuguese, spoken as Brazilian: the published voices are Brazilian, and
    /// a European voice fed Brazilian phonemes is worse than not offering it.
    Pt,
}

impl Lang {
    pub const ALL: [Lang; 6] = [Lang::Ru, Lang::En, Lang::De, Lang::Fr, Lang::Es, Lang::Pt];

    /// ISO 639-1 code, as stored in the database and written in config.
    pub fn code(self) -> &'static str {
        match self {
            Lang::Ru => "ru",
            Lang::En => "en",
            Lang::De => "de",
            Lang::Fr => "fr",
            Lang::Es => "es",
            Lang::Pt => "pt",
        }
    }

    /// English name of the language. Naming the target language explicitly is
    /// what keeps Qwen3 from drifting into English when the source text
    /// contains stray foreign words; a vague "the language of the article"
    /// instruction is not enough.
    pub fn name(self) -> &'static str {
        match self {
            Lang::Ru => "Russian",
            Lang::En => "English",
            Lang::De => "German",
            Lang::Fr => "French",
            Lang::Es => "Spanish",
            Lang::Pt => "Brazilian Portuguese",
        }
    }

    /// Snowball stemmer for keyword matching.
    pub fn stemmer(self) -> rust_stemmers::Algorithm {
        use rust_stemmers::Algorithm;
        match self {
            Lang::Ru => Algorithm::Russian,
            Lang::En => Algorithm::English,
            Lang::De => Algorithm::German,
            Lang::Fr => Algorithm::French,
            Lang::Es => Algorithm::Spanish,
            Lang::Pt => Algorithm::Portuguese,
        }
    }

    /// Map a detected `whatlang` language onto one of ours, if it is one.
    ///
    /// The quality gate discards an article whose language disagrees with its
    /// source, so confusing Spanish and Portuguese would lose articles silently.
    /// Over 336 items from Spanish and Brazilian outlets it confused none.
    pub fn from_whatlang(l: whatlang::Lang) -> Option<Lang> {
        match l {
            whatlang::Lang::Rus => Some(Lang::Ru),
            whatlang::Lang::Eng => Some(Lang::En),
            whatlang::Lang::Deu => Some(Lang::De),
            whatlang::Lang::Fra => Some(Lang::Fr),
            whatlang::Lang::Spa => Some(Lang::Es),
            whatlang::Lang::Por => Some(Lang::Pt),
            _ => None,
        }
    }

    /// Best-effort detection of the language a piece of text is written in.
    pub fn detect(text: &str) -> Option<Lang> {
        whatlang::detect(text).and_then(|info| {
            // A low-confidence guess on a short snippet is worse than none: it
            // makes the quality gate reject a perfectly good article.
            if info.confidence() < 0.5 { None } else { Lang::from_whatlang(info.lang()) }
        })
    }
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl FromStr for Lang {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ru" | "rus" | "russian" => Ok(Lang::Ru),
            "en" | "eng" | "english" => Ok(Lang::En),
            "de" | "deu" | "ger" | "german" => Ok(Lang::De),
            "fr" | "fra" | "fre" | "french" => Ok(Lang::Fr),
            "es" | "spa" | "spanish" => Ok(Lang::Es),
            "pt" | "pt-br" | "por" | "portuguese" => Ok(Lang::Pt),
            other => {
                Err(format!("unsupported language '{other}' (expected ru, en, de, fr, es or pt)"))
            }
        }
    }
}

/// The instruction appended to every system prompt to pin the output language.
pub fn directive(target: Lang) -> String {
    let name = target.name();
    format!(
        "IMPORTANT: Write the ENTIRE output in {name} only. The source text may be in another \
         language and may contain foreign words, brand names or people's names; translate the \
         content into {name} regardless, and transliterate names where {name} normally does. \
         Do not switch languages even once, not for a single word."
    )
}

/// Distinct runs of two or more ASCII letters — used to detect stray Latin words
/// left in an otherwise-Cyrillic summary.
pub fn latin_words(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_ascii_alphabetic() {
            cur.push(c);
        } else {
            if cur.chars().count() >= 2 {
                out.push(std::mem::take(&mut cur));
            }
            cur.clear();
        }
    }
    if cur.chars().count() >= 2 {
        out.push(cur);
    }
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_aliases() {
        assert_eq!("RU".parse::<Lang>().unwrap(), Lang::Ru);
        assert_eq!("german".parse::<Lang>().unwrap(), Lang::De);
        // Portuguese answers to the tag the interface uses for it, so a value
        // copied from `ui_lang` into `output_lang` by hand still parses.
        assert_eq!("pt-BR".parse::<Lang>().unwrap(), Lang::Pt);
        assert_eq!("spanish".parse::<Lang>().unwrap(), Lang::Es);
        assert!("it".parse::<Lang>().is_err(), "итальянского у нас нет — и врать об этом нельзя");
    }

    #[test]
    fn finds_stray_latin() {
        let words = latin_words("но presently в Амстердаме и a там");
        assert_eq!(words, vec!["presently".to_string()]);
    }
}
