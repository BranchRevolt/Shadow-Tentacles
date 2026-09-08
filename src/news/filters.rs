// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Keyword and date filtering.
//!
//! Keywords match on stems rather than substrings, so `санкции` finds
//! `санкциями` and `Wahl` does not drag in `Auswahl`.

use chrono::{DateTime, Utc};
use rust_stemmers::Stemmer;

use crate::llm::Lang;

/// A compiled keyword query.
///
/// One entry per line or comma. An entry is a group of stems that must all
/// appear; between entries it is "or", and a leading `-` vetoes.
pub struct KeywordFilter {
    include: Vec<Vec<String>>,
    exclude: Vec<Vec<String>>,
    stemmer: Stemmer,
}

impl KeywordFilter {
    /// Build from the reader's words. An entry prefixed with `-` excludes.
    pub fn new(keywords: &[String], lang: Lang) -> KeywordFilter {
        let stemmer = Stemmer::create(lang.stemmer());
        let mut include = Vec::new();
        let mut exclude = Vec::new();

        for raw in keywords {
            let raw = raw.trim();
            let (target, entry) = match raw.strip_prefix('-') {
                Some(rest) => (&mut exclude, rest),
                None => (&mut include, raw),
            };
            let group: Vec<String> = entry
                .split_whitespace()
                .map(|part| stem_word(&stemmer, part))
                .filter(|stem| !stem.is_empty())
                .collect();
            if !group.is_empty() {
                target.push(group);
            }
        }

        KeywordFilter { include, exclude, stemmer }
    }

    /// A short name for exactly this set of words. Order and spacing do not
    /// count, the stems do.
    pub fn fingerprint(&self) -> String {
        let render = |sign: char, group: &Vec<String>| format!("{sign}{}", group.join(" "));
        let mut parts: Vec<String> = self
            .include
            .iter()
            .map(|g| render('+', g))
            .chain(self.exclude.iter().map(|g| render('-', g)))
            .collect();
        parts.sort();

        // FNV-1a: enough to tell two word lists apart, and no dependency.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in parts.join(",").bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
        format!("{hash:016x}")
    }

    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Does this text interest the reader?
    ///
    /// With no include list, everything matches — an empty keyword list means
    /// "show me the news", not "show me nothing".
    pub fn matches(&self, text: &str) -> bool {
        if self.is_empty() {
            return true;
        }
        let stems = stems_of(&self.stemmer, text);
        let present = |group: &Vec<String>| group.iter().all(|stem| stems.contains(stem));

        if self.exclude.iter().any(present) {
            return false;
        }
        if self.include.is_empty() {
            return true;
        }
        self.include.iter().any(present)
    }
}

/// Every stem a piece of text offers, taking both readings of a token with
/// punctuation inside it.
///
/// Stripping punctuation from "AI-driven" gives "aidriven", which "AI" never
/// matches; splitting gives "ai" and "driven", but breaks "A.I." Both are kept.
fn stems_of(stemmer: &Stemmer, text: &str) -> std::collections::HashSet<String> {
    let mut stems = std::collections::HashSet::new();
    for token in text.split_whitespace() {
        let joined = stem_word(stemmer, token);
        if !joined.is_empty() {
            stems.insert(joined);
        }
        for part in token.split(|c: char| !c.is_alphanumeric()) {
            let stem = stem_word(stemmer, part);
            if !stem.is_empty() {
                stems.insert(stem);
            }
        }
    }
    stems
}

fn stem_word(stemmer: &Stemmer, word: &str) -> String {
    let cleaned: String =
        word.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    if cleaned.is_empty() {
        return String::new();
    }
    stemmer.stem(&cleaned).into_owned()
}

/// A publication-date window. Both ends optional.
#[derive(Debug, Clone, Copy, Default)]
pub struct DateRange {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
}

impl DateRange {
    /// The last `days` days.
    pub fn last_days(days: i64) -> DateRange {
        DateRange { since: Some(Utc::now() - chrono::Duration::days(days)), until: None }
    }

    pub fn is_open(&self) -> bool {
        self.since.is_none() && self.until.is_none()
    }

    /// Does an article with this date belong in the window?
    ///
    /// An unknown date passes: publishers omit and mangle dates often enough
    /// that excluding them would hide real articles. The card says the date is
    /// uncertain instead.
    pub fn contains(&self, when: Option<DateTime<Utc>>) -> bool {
        let Some(when) = when else { return true };
        if let Some(since) = self.since
            && when < since
        {
            return false;
        }
        if let Some(until) = self.until
            && when > until
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(words: &[&str], lang: Lang) -> KeywordFilter {
        KeywordFilter::new(&words.iter().map(|s| s.to_string()).collect::<Vec<_>>(), lang)
    }

    #[test]
    fn an_entry_of_two_words_means_one_subject() {
        let filter = f(&["локальные нейросети"], Lang::Ru);
        assert!(filter.matches("как запустить локальные нейросети дома"));
        // Both halves, or it is not that subject. Before this was grouped, a
        // local bus service matched a search for local language models.
        assert!(!filter.matches("локальный автобус изменил маршрут"));
        assert!(!filter.matches("нейросети в медицине"));
    }

    #[test]
    fn separate_entries_are_alternatives() {
        let filter = f(&["санкции", "выборы"], Lang::Ru);
        assert!(filter.matches("обсуждают санкции"));
        assert!(filter.matches("прошли выборы"));
        assert!(!filter.matches("сегодня тепло и солнечно"));
    }

    #[test]
    fn a_word_glued_to_another_by_punctuation_is_still_found() {
        let filter = f(&["AI"], Lang::En);
        assert!(filter.matches("an AI-driven newsroom"), "дефис не должен прятать слово");
        assert!(filter.matches("the A.I. department"), "и точки внутри тоже");
        assert!(filter.matches("plain AI here"));
        assert!(!filter.matches("nothing of the sort"));
    }

    #[test]
    fn an_excluded_phrase_needs_all_of_its_words() {
        let filter = f(&["-зимний спорт"], Lang::Ru);
        // The veto is a subject too: excluding "зимний спорт" must not throw
        // away everything mentioning winter.
        assert!(filter.matches("зимний вечер в городе"));
        assert!(!filter.matches("зимний спорт набирает популярность"));
    }

    #[test]
    fn the_fingerprint_follows_the_meaning_not_the_typing() {
        // Same words, differently written: the same question, so verdicts
        // recorded against the first must still apply.
        assert_eq!(
            f(&["санкции", "-спорт"], Lang::Ru).fingerprint(),
            f(&["  -спорт ", "санкциями"], Lang::Ru).fingerprint()
        );
        // A word added is a different question, and everything decided under
        // the old one has to be asked again.
        assert_ne!(
            f(&["санкции"], Lang::Ru).fingerprint(),
            f(&["санкции", "выборы"], Lang::Ru).fingerprint()
        );
        // And an exclusion is not the same as an inclusion.
        assert_ne!(f(&["спорт"], Lang::Ru).fingerprint(), f(&["-спорт"], Lang::Ru).fingerprint());
    }

    #[test]
    fn russian_inflections_match() {
        let filter = f(&["санкции"], Lang::Ru);
        assert!(filter.matches("новые санкциями против компании"));
        assert!(filter.matches("обсуждают санкция"));
        assert!(!filter.matches("совершенно другая новость про погоду"));
    }

    #[test]
    fn german_inflections_match() {
        let filter = f(&["Wahlen"], Lang::De);
        assert!(filter.matches("Die Wahl war knapp"));
    }

    #[test]
    fn empty_filter_lets_everything_through() {
        assert!(f(&[], Lang::Ru).matches("что угодно"));
    }

    #[test]
    fn exclusions_veto_a_match() {
        let filter = f(&["банк", "-спорт"], Lang::Ru);
        assert!(filter.matches("банк повысил ставку"));
        assert!(!filter.matches("банк спонсирует спорт"));
    }

    #[test]
    fn only_exclusions_still_filters() {
        let filter = f(&["-реклама"], Lang::Ru);
        assert!(filter.matches("обычная новость"));
        assert!(!filter.matches("это реклама"));
    }

    #[test]
    fn punctuation_does_not_hide_a_word() {
        assert!(f(&["ставка"], Lang::Ru).matches("повысил «ставку»!"));
    }

    #[test]
    fn open_range_accepts_everything() {
        assert!(DateRange::default().contains(Some(Utc::now())));
        assert!(DateRange::default().is_open());
    }

    #[test]
    fn window_excludes_older_articles() {
        let range = DateRange::last_days(3);
        assert!(range.contains(Some(Utc::now() - chrono::Duration::days(1))));
        assert!(!range.contains(Some(Utc::now() - chrono::Duration::days(10))));
    }

    #[test]
    fn unknown_dates_are_kept() {
        assert!(DateRange::last_days(1).contains(None));
    }
}
