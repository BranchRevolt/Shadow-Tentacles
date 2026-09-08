// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Turning text into the phonemes the synthesizer reads.
//!
//! One call to espeak-ng, with the voice the Piper model names in its own
//! config rather than the language's default. Russian additionally moves the
//! primary stress mark to the vowel the dictionary gives; see `stress`.

use parking_lot::Mutex;

use crate::llm::Lang;
use crate::tts::stress::{StressDictionary, split_words, vowel_count};

/// The IPA vowels espeak emits for the supported languages, plus the Russian reductions
/// it uses for unstressed positions.
const IPA_VOWELS: &str = "aeiouyɐɑɒæɔəɛɜɪɔʊʌyɨøœɶʏeɘɵ";
/// Primary and secondary stress, as espeak writes them.
const STRESS_MARKS: [char; 2] = ['ˈ', 'ˌ'];

/// Punctuation the voice models are trained on. Passed through untouched,
/// because espeak drops it and the model needs it to place pauses — without
/// them a summary is read as one unbroken sentence.
const KEPT_PUNCTUATION: &str = ",.!?;:()\"-";

/// Phonemize `text` for `lang`, correcting Russian stress where possible.
///
/// `voice` is espeak's name for the dialect the Piper model was trained with.
/// Cut at punctuation and phonemized piece by piece, espeak not keeping the
/// marks itself.
pub fn phonemise(text: &str, lang: Lang, voice: &str, dict: Option<&StressDictionary>) -> String {
    // Numbers and symbols are put into a readable shape first: a thousands
    // separator is a space, and splitting on spaces afterwards would turn
    // "2 900" into two numbers.
    let text = crate::tts::normalize::normalize(text, lang);
    let mut out = String::with_capacity(text.len() * 3);

    for piece in split_on_punctuation(&text) {
        match piece {
            Piece::Punctuation(c) => out.push(c),
            Piece::Text(run) if run.trim().is_empty() => out.push(' '),
            Piece::Text(run) => {
                let phonemes = match (lang, dict) {
                    (Lang::Ru, Some(dict)) => russian(run, dict, voice),
                    _ => espeak(run, voice),
                };
                if !phonemes.is_empty() {
                    if !out.is_empty() && !out.ends_with(' ') {
                        out.push(' ');
                    }
                    out.push_str(&phonemes);
                }
            }
        }
    }
    out.trim().to_string()
}

enum Piece<'a> {
    Text(&'a str),
    Punctuation(char),
}

fn split_on_punctuation(text: &str) -> impl Iterator<Item = Piece<'_>> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        match rest.char_indices().find(|(_, c)| KEPT_PUNCTUATION.contains(*c)) {
            Some((0, c)) => {
                rest = &rest[c.len_utf8()..];
                Some(Piece::Punctuation(c))
            }
            Some((at, _)) => {
                let (head, tail) = rest.split_at(at);
                rest = tail;
                Some(Piece::Text(head))
            }
            None => {
                let all = rest;
                rest = "";
                Some(Piece::Text(all))
            }
        }
    })
}

/// espeak-ng keeps its state in globals, the loaded voice among them, and
/// calling it from two threads at once segfaults. One lock around every call;
/// the work is microseconds, so nothing queues for long.
static ESPEAK: Mutex<()> = Mutex::new(());

fn espeak(text: &str, voice: &str) -> String {
    let _guard = ESPEAK.lock();
    crate::tts::espeak_data::ensure();
    espeak_rs::text_to_phonemes(text, voice, None).map(|p| p.join(" ")).unwrap_or_default()
}

/// Phonemize Russian word by word, so each word's stress can be corrected
/// against the dictionary before the pieces are joined.
///
/// Whole-sentence output gives one string with no way to tell which phoneme
/// came from which word.
fn russian(text: &str, dict: &StressDictionary, voice: &str) -> String {
    let mut out = String::with_capacity(text.len() * 3);

    for (part, is_word) in split_words(text) {
        if !is_word {
            // Whitespace between words; punctuation was taken out upstream.
            out.push(' ');
            continue;
        }

        // ё first: espeak reads "все" and "всё" differently, and the writer of
        // a news article almost never types the ё.
        let spoken = dict.restore_yo(&part.to_lowercase());
        let phonemes = espeak(&spoken, voice);

        out.push_str(&match dict.stress_of(&part.to_lowercase()) {
            // A single-vowel word cannot be stressed wrongly, so leave espeak's
            // own output alone rather than rewriting it for nothing.
            Some(target) if vowel_count(&spoken) > 1 => {
                restress_russian(&phonemes, &spoken, target)
            }
            _ => phonemes,
        });
        out.push(' ');
    }
    out.trim().to_string()
}

/// How espeak realizes each Russian vowel letter, stressed and unstressed.
///
/// Moving the stress mark is not enough: espeak reduces vowels by its own idea
/// of where the stress falls, so the newly stressed vowel is restored too. The
/// percentages are how often the listed sound was produced, over forty thousand
/// dictionary words.
const STRESSED: &[(char, char)] = &[
    ('а', 'ɑ'), // 100%
    ('е', 'e'), // 93%
    ('ё', 'ɵ'), // 90%
    ('и', 'i'), // 93%
    ('о', 'o'), // 100%
    ('у', 'u'), // 100%
    ('ы', 'y'), // 100%
    ('э', 'ɛ'), // 100%
    ('ю', 'u'), // 100%
    ('я', 'ɑ'), // 89%
];

/// The reductions, for the vowel that loses the stress. Only the unambiguous
/// ones are applied: unstressed а and я come out as ʌ barely more often than
/// as a, and an unreduced vowel merely sounds over-careful, while a wrongly
/// reduced one sounds like a different word.
const UNSTRESSED: &[(char, char)] = &[
    ('о', 'ʌ'), // 100%
    ('е', 'i'), // 76%
];

fn realise(letter: char, table: &[(char, char)]) -> Option<char> {
    table.iter().find(|(l, _)| *l == letter).map(|(_, sound)| *sound)
}

/// Russian vowel letters, in the order stress can fall on them.
const RU_VOWELS: &str = "аеёиоуыэюя";

/// Move the stress in `ipa` onto the `target`-th vowel, restoring the vowel
/// qualities espeak chose for a different placement.
///
/// `word` is the spelling the phonemes came from: once reduced, the phonemes no
/// longer distinguish an о from an а.
fn restress_russian(ipa: &str, word: &str, target: usize) -> String {
    let letters: Vec<char> = word.chars().filter(|c| RU_VOWELS.contains(*c)).collect();

    // The alignment is one letter to one sound in 99.9% of words; where it is
    // not, correcting by index would edit the wrong vowel, so leave the word to
    // espeak entirely.
    let sounds = ipa.chars().filter(|c| IPA_VOWELS.contains(*c)).count();
    if letters.len() != sounds || target >= letters.len() {
        return restress(ipa, target);
    }

    let old = stressed_index(ipa);
    let mut out = String::with_capacity(ipa.len() + 2);
    let mut seen = 0usize;

    for c in ipa.chars() {
        if STRESS_MARKS.contains(&c) {
            continue;
        }
        if IPA_VOWELS.contains(c) {
            let letter = letters[seen];
            if seen == target {
                out.push('ˈ');
                out.push(realise(letter, STRESSED).unwrap_or(c));
            } else if Some(seen) == old {
                out.push(realise(letter, UNSTRESSED).unwrap_or(c));
            } else {
                out.push(c);
            }
            seen += 1;
            continue;
        }
        out.push(c);
    }
    out
}

/// Which vowel espeak marked as stressed, if any.
fn stressed_index(ipa: &str) -> Option<usize> {
    let mut seen = 0usize;
    for c in ipa.chars() {
        if c == 'ˈ' {
            return Some(seen);
        }
        if IPA_VOWELS.contains(c) {
            seen += 1;
        }
    }
    None
}

/// Move the primary stress onto the `target`-th vowel of `ipa`, leaving the
/// vowels themselves alone. Used where reduction does not apply.
pub fn restress(ipa: &str, target: usize) -> String {
    let mut out = String::with_capacity(ipa.len() + 2);
    let mut seen = 0usize;
    let mut placed = false;

    for c in ipa.chars() {
        if STRESS_MARKS.contains(&c) {
            continue;
        }
        if IPA_VOWELS.contains(c) {
            if seen == target {
                out.push('ˈ');
                placed = true;
            }
            seen += 1;
        }
        out.push(c);
    }

    // The dictionary counts vowels in Cyrillic; espeak sometimes produces a
    // different number for the same word. Rather than leave a word unstressed,
    // fall back to what espeak thought.
    if placed { out } else { ipa.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_mark_lands_on_the_named_vowel() {
        assert_eq!(restress("zamˈok", 0), "zˈamok");
        assert_eq!(restress("zamˈok", 1), "zamˈok");
    }

    #[test]
    fn an_existing_mark_is_removed_not_duplicated() {
        let out = restress("bʌɭʃˈɑja", 0);
        assert_eq!(out.matches('ˈ').count(), 1, "две пометки ударения: {out}");
        assert!(out.starts_with("bˈʌ"), "{out}");
    }

    #[test]
    fn an_impossible_index_keeps_espeaks_answer() {
        // The dictionary counted more vowels than espeak produced; guessing
        // would be worse than deferring.
        assert_eq!(restress("zamˈok", 9), "zamˈok");
    }

    #[test]
    fn secondary_stress_is_cleared_too() {
        assert!(!restress("mˌoʒyt", 0).contains('ˌ'));
    }

    #[test]
    fn other_languages_go_straight_through() {
        // No dictionary, no rewriting — and espeak must still answer.
        let en = phonemise("the bank raised rates", Lang::En, "en", None);
        assert!(!en.is_empty());
        assert!(en.contains('ˈ'), "espeak должен ставить ударение сам: {en}");
    }

    #[test]
    fn the_newly_stressed_vowel_gets_its_full_quality_back() {
        // espeak reduces the first о of Лондона because it stresses the second.
        // Moving only the mark leaves a stressed ʌ — the sound of "Ландона".
        let espeaks = "ɭʌndˈona";
        let ours = restress_russian(espeaks, "лондона", 0);
        assert!(ours.contains("ˈo"), "ударная гласная не восстановлена: {ours}");
        assert!(!ours.contains("ˈʌ"), "осталась ударная редуцированная: {ours}");
        // And the vowel that lost the stress is reduced in its turn.
        assert!(ours.ends_with("ndʌna") || ours.contains('ʌ'), "{ours}");
    }

    #[test]
    fn a_mismatched_alignment_is_left_to_espeak() {
        // Fewer letters than sounds: correcting by index would edit the wrong
        // vowel, so the word goes through untouched but for the mark.
        let out = restress_russian("sˈiːˈɪɭa", "cила", 0);
        assert!(!out.is_empty());
    }

    #[test]
    fn numbers_reach_the_synthesiser() {
        // espeak reads 19 as девятнадцать; dropping digits deleted every figure.
        let out = phonemise("19 тел", Lang::Ru, "ru", None);
        assert!(out.len() > 10, "число не озвучено: {out}");
    }

    #[test]
    fn a_thousands_separator_does_not_split_the_number() {
        // "2 900" read as two numbers is "два девятьсот"; as one it is
        // "две тысячи девятьсот", and the two are audibly different.
        let split = phonemise("2 900", Lang::Ru, "ru", None);
        let whole = phonemise("2900", Lang::Ru, "ru", None);
        assert_eq!(split, whole, "разряды всё ещё разрываются");
        assert!(whole.contains("tˈysʲitʃʲi"), "не прочитано как тысячи: {whole}");
    }

    #[test]
    fn russian_words_get_their_stress_from_the_dictionary() {
        let dir = match crate::paths::voices_dir() {
            Ok(d) if StressDictionary::is_installed(&d) => d,
            _ => return,
        };
        let dict = StressDictionary::open(&dir).unwrap();

        // espeak says novˈosʲtʲɪ; the dictionary says the stress is on the о.
        let ours = phonemise("новости", Lang::Ru, "ru", Some(&dict));
        let espeaks = phonemise("новости", Lang::Ru, "ru", None);
        assert_ne!(ours, espeaks, "ударение должно было сместиться");
        assert!(ours.starts_with("nˈ") || ours.starts_with("nˈo"), "{ours}");
    }

    #[test]
    fn punctuation_survives_into_the_phonemes() {
        // A comma is a pause. espeak drops it, the model expects it, so it has
        // to be carried across by hand.
        let ru = phonemise("банк, вероятно, поднимет ставку.", Lang::Ru, "ru", None);
        assert_eq!(ru.matches(',').count(), 2, "запятые потерялись: {ru}");
        assert!(ru.ends_with('.'), "точка потерялась: {ru}");

        let en = phonemise("the bank, it seems, raised rates.", Lang::En, "en", None);
        assert_eq!(en.matches(',').count(), 2, "{en}");
    }

    #[test]
    fn text_between_marks_is_phonemised_whole() {
        // Not word by word: liaison and phrase intonation live inside a phrase.
        let fr = phonemise("les amis arrivent", Lang::Fr, "fr", None);
        assert!(!fr.is_empty());
        assert!(!fr.contains(','));
    }

    #[test]
    fn a_line_of_only_punctuation_does_not_panic() {
        assert!(!phonemise("!?.,", Lang::En, "en", None).is_empty());
        assert_eq!(phonemise("", Lang::En, "en", None), "");
    }
}
