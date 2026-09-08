// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Recognizing the same story twice.
//!
//! Two layers: canonicalizing the address, and SimHash over the text.
//! Duplicates are grouped, never deleted.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use url::Url;

/// Query parameters that never identify content, only where a click came from.
const TRACKING_PREFIXES: &[&str] = &["utm_", "yclid", "gclid", "fbclid", "_openstat", "ref_"];

/// Normalize a URL so two links to the same page compare equal.
///
/// Conservative: dropping a parameter that selects content, such as a page
/// number, would merge two different stories, which is worse than missing a
/// duplicate.
pub fn canonicalize(raw: &str) -> String {
    let Ok(mut url) = Url::parse(raw) else {
        return raw.trim().trim_end_matches('/').to_string();
    };

    // Scheme and host are case-insensitive; the path is not.
    if url.scheme() == "http" {
        let _ = url.set_scheme("https");
    }
    if let Some(host) = url.host_str() {
        let host = host.trim_start_matches("www.").to_lowercase();
        let _ = url.set_host(Some(&host));
    }

    let kept: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(k, _)| {
            let k = k.to_lowercase();
            !TRACKING_PREFIXES.iter().any(|p| k.starts_with(p))
        })
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    if kept.is_empty() {
        url.set_query(None);
    } else {
        let mut sorted = kept;
        // Parameter order is not meaningful; sorting makes the form canonical.
        sorted.sort();
        let mut pairs = url.query_pairs_mut();
        pairs.clear();
        for (k, v) in sorted {
            pairs.append_pair(&k, &v);
        }
        drop(pairs);
    }

    // A fragment addresses a place within one page, never a different page.
    url.set_fragment(None);

    let s = url.to_string();
    s.strip_suffix('/').unwrap_or(&s).to_string()
}

/// 64-bit SimHash over word shingles.
///
/// Word-level rather than character-level: these languages inflect heavily, and
/// character shingles would score unrelated texts as similar for sharing
/// endings.
pub fn simhash(text: &str) -> u64 {
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.to_lowercase())
        .filter(|w| w.chars().count() > 2)
        .collect();

    // SimHash needs a document, not a sentence: with only a handful of shingles
    // a single edited word flips a dozen bits, so short texts score as unrelated
    // no matter how similar they are. Returning 0 says "not comparable", which
    // `is_near_duplicate` already treats as never matching — better an honest
    // abstention than a confident wrong answer.
    if words.len() < MIN_SHINGLE_WORDS {
        return 0;
    }

    let mut bits = [0i32; 64];
    // Pairs of adjacent words: enough context to tell "bank raised rates" from
    // "rates raised bank", cheap enough to compute on every article.
    for pair in words.windows(2) {
        let mut hasher = DefaultHasher::new();
        pair.hash(&mut hasher);
        let h = hasher.finish();
        for (i, bit) in bits.iter_mut().enumerate() {
            if h >> i & 1 == 1 {
                *bit += 1;
            } else {
                *bit -= 1;
            }
        }
    }

    bits.iter().enumerate().fold(0u64, |acc, (i, &b)| if b > 0 { acc | 1 << i } else { acc })
}

/// Number of differing bits. Below ~4 the texts are near-identical; below ~8
/// they are the same story with edits.
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Fewer words than this and SimHash carries no signal. A real news article
/// clears it several times over; a headline or a stub does not.
const MIN_SHINGLE_WORDS: usize = 60;

/// Distance at which two articles count as the same story.
pub const NEAR_DUPLICATE_BITS: u32 = 6;

pub fn is_near_duplicate(a: u64, b: u64) -> bool {
    // A zero hash means "no usable text"; two of those are not evidence of
    // anything and must not collapse every empty article into one cluster.
    a != 0 && b != 0 && hamming(a, b) <= NEAR_DUPLICATE_BITS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracking_parameters_are_dropped() {
        assert_eq!(
            canonicalize("https://example.com/a?utm_source=tg&id=7"),
            canonicalize("https://example.com/a?id=7")
        );
    }

    #[test]
    fn scheme_www_and_trailing_slash_are_normalised() {
        assert_eq!(
            canonicalize("http://www.Example.com/a/"),
            canonicalize("https://example.com/a")
        );
    }

    #[test]
    fn meaningful_parameters_are_kept() {
        assert_ne!(
            canonicalize("https://example.com/a?id=7"),
            canonicalize("https://example.com/a?id=8")
        );
    }

    #[test]
    fn parameter_order_does_not_matter() {
        assert_eq!(
            canonicalize("https://example.com/a?b=2&a=1"),
            canonicalize("https://example.com/a?a=1&b=2")
        );
    }

    /// An article-length body: SimHash is meaningless on anything shorter, and
    /// testing it on a sentence would only measure the noise floor.
    fn body(subject: &str, tail: &str) -> String {
        let mut s = String::new();
        for i in 0..12 {
            s.push_str(&format!(
                "{subject} сообщили представители ведомства в пункте {i} официального заявления, \
                 опубликованного сегодня на сайте организации для широкой публики. "
            ));
        }
        s.push_str(tail);
        s
    }

    #[test]
    fn reprints_are_near_duplicates() {
        let a =
            body("Центральный банк повысил ключевую ставку до восьми процентов годовых,", "утром");
        let b =
            body("Центральный банк повысил ключевую ставку до восьми процентов годовых,", "днём");
        assert!(
            is_near_duplicate(simhash(&a), simhash(&b)),
            "distance {}",
            hamming(simhash(&a), simhash(&b))
        );
    }

    #[test]
    fn different_stories_are_not_duplicates() {
        let a = body("Центральный банк повысил ключевую ставку до восьми процентов годовых,", "");
        let b = body("Городская администрация открыла новый парк в центре города для жителей,", "");
        assert!(!is_near_duplicate(simhash(&a), simhash(&b)));
    }

    #[test]
    fn short_texts_abstain_rather_than_guess() {
        // Two identical sentences would hash the same, but a sentence carries no
        // SimHash signal, so this refuses to judge instead of claiming a match.
        let s = "Центральный банк повысил ключевую ставку до восьми процентов";
        assert_eq!(simhash(s), 0);
        assert!(!is_near_duplicate(simhash(s), simhash(s)));
    }

    #[test]
    fn empty_texts_never_collapse_together() {
        assert!(!is_near_duplicate(simhash(""), simhash("")));
    }
}
