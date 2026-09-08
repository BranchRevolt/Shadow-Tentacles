// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Russian stress placement, and the ё restoration that precedes it.
//!
//! espeak-ng's own placement is wrong often enough to be unlistenable, and the
//! text cannot carry a correction: it reads `+` aloud and ignores the combining
//! acute, so the mark is moved in the phonemes instead. The dictionary is
//! memory-mapped and searched rather than parsed, three million word forms being
//! too much to hold resident.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;

use memmap2::Mmap;

use crate::error::{AppError, AppResult};

/// Vowels of the Russian alphabet, in the order stress can fall on them.
const VOWELS: &str = "аеёиоуыэюя";

pub struct StressDictionary {
    /// Sorted `word\tN` lines: the word, and which of its vowels is stressed.
    accents: Mmap,
    /// Sorted `spelling\tspelling-with-ё` lines.
    yo: Mmap,
}

impl StressDictionary {
    pub fn open(dir: &Path) -> AppResult<StressDictionary> {
        Ok(StressDictionary {
            accents: map_file(&dir.join("ru-accents.tsv"))?,
            yo: map_file(&dir.join("ru-yo.tsv"))?,
        })
    }

    pub fn is_installed(dir: &Path) -> bool {
        dir.join("ru-accents.tsv").is_file() && dir.join("ru-yo.tsv").is_file()
    }

    /// Which vowel of `word` carries the stress, counting from zero.
    ///
    /// Both spellings are tried: the dictionary is inconsistent about ё, listing
    /// some words with е and others only with ё. The index counts vowels in
    /// whichever spelling matched, and ё is a vowel in both.
    pub fn stress_of(&self, word: &str) -> Option<usize> {
        let plain = word.to_lowercase();
        if let Some(value) = lookup(&self.accents, &plain) {
            return value.parse().ok();
        }
        let restored = self.restore_yo(&plain);
        if restored == plain {
            return None;
        }
        lookup(&self.accents, &restored)?.parse().ok()
    }

    /// Put the ё back where the writer left an е.
    ///
    /// Not cosmetic — "все" and "всё" are different words — and it has to run
    /// before the stress lookup, the dictionary being keyed on the ё spelling.
    pub fn restore_yo(&self, word: &str) -> String {
        if !word.contains('е') {
            return word.to_string();
        }
        lookup(&self.yo, word).unwrap_or(word).to_string()
    }

    /// Rewrite a whole sentence with ё restored, leaving everything else alone.
    pub fn restore_yo_in(&self, text: &str) -> String {
        split_words(text)
            .map(|(part, is_word)| {
                if !is_word {
                    return part.to_string();
                }
                let restored = self.restore_yo(&part.to_lowercase());
                match_case(part, &restored)
            })
            .collect()
    }
}

fn map_file(path: &Path) -> AppResult<Mmap> {
    let file = File::open(path).map_err(|e| AppError::Other(format!("{}: {e}", path.display())))?;
    // Safety: the file is ours, written once at install time and read-only
    // afterwards. A truncation under the map would be a torn read, which is
    // why installation writes to a temporary name and renames.
    unsafe { Mmap::map(&file) }.map_err(|e| AppError::Other(format!("{}: {e}", path.display())))
}

/// Binary search a sorted `key\tvalue` file without loading it.
fn lookup<'a>(data: &'a [u8], key: &str) -> Option<&'a str> {
    let (mut low, mut high) = (0usize, data.len());

    while low < high {
        let mid = (low + high) / 2;
        // Land on a line boundary: step back to the start of the line `mid`
        // fell inside.
        let start = data[..mid].iter().rposition(|b| *b == b'\n').map(|i| i + 1).unwrap_or(0);
        let end =
            data[start..].iter().position(|b| *b == b'\n').map(|i| start + i).unwrap_or(data.len());

        let line = std::str::from_utf8(&data[start..end]).ok()?;
        let (word, value) = line.split_once('\t')?;

        match word.cmp(key) {
            std::cmp::Ordering::Equal => return Some(value),
            std::cmp::Ordering::Less => {
                // Everything up to and including this line is too small.
                if end >= high {
                    return None;
                }
                low = end + 1;
            }
            std::cmp::Ordering::Greater => {
                if start == 0 {
                    return None;
                }
                high = start - 1;
            }
        }
    }
    None
}

/// Split text into runs of letters and runs of everything else.
pub fn split_words(text: &str) -> impl Iterator<Item = (&str, bool)> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let is_word = rest.chars().next().is_some_and(is_word_char);
        let end = rest
            .char_indices()
            .find(|(_, c)| is_word_char(*c) != is_word)
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        let (part, tail) = rest.split_at(end);
        rest = tail;
        Some((part, is_word))
    })
}

fn is_word_char(c: char) -> bool {
    // Digits belong to a spoken token: espeak reads "19" as девятнадцать, and
    // excluding them here dropped every number out of the audio in silence.
    c.is_alphanumeric() || c == '-'
}

/// Give `replacement` the capitalisation of `original`.
fn match_case(original: &str, replacement: &str) -> String {
    let mut chars = original.chars();
    match chars.next() {
        Some(first) if first.is_uppercase() => {
            let mut out: String =
                replacement.chars().take(1).flat_map(char::to_uppercase).collect();
            out.extend(replacement.chars().skip(1));
            out
        }
        _ => replacement.to_string(),
    }
}

/// Count the vowels in a Russian word, for checking a stress index is sane.
pub fn vowel_count(word: &str) -> usize {
    word.chars().filter(|c| VOWELS.contains(*c)).count()
}

/// Where RUAccent publishes the two dictionaries, and what they weigh.
///
/// Twenty-one megabytes against a sixty-three megabyte voice, so it is fetched
/// with the voice rather than offered as a separate choice.
pub const ACCENTS_URL: &str =
    "https://huggingface.co/ruaccent/accentuator/resolve/main/dictionary/accents.json.gz";
pub const YO_URL: &str =
    "https://huggingface.co/ruaccent/accentuator/resolve/main/dictionary/yo_words.json.gz";
/// What the two downloads add up to, for a caller that wants to say so first.
pub const DOWNLOAD_BYTES: u64 = 20_954_156 + 548_914;

/// Turn RUAccent's dictionaries into the two files this module maps.
///
/// Done once, when the Russian voice is installed. The source is JSON of three
/// million entries — parsing that on every start would cost seconds and a
/// hundred megabytes; a sorted table costs neither.
pub fn install(accents_json: &Path, yo_json: &Path, into: &Path) -> AppResult<()> {
    std::fs::create_dir_all(into)?;
    write_table(accents_json, &into.join("ru-accents.tsv"), |marked| {
        // "минист+ерство" — the marker sits before the stressed vowel, so
        // want its ordinal among the word's vowels.
        let cut = marked.find('+')?;
        Some(vowel_count(&marked[..cut]).to_string())
    })?;
    write_table(yo_json, &into.join("ru-yo.tsv"), |value| Some(value.to_string()))?;
    Ok(())
}

fn write_table(
    source: &Path,
    target: &Path,
    convert: impl Fn(&str) -> Option<String>,
) -> AppResult<()> {
    let file = File::open(source)?;
    let reader = flate2::read::GzDecoder::new(file);
    let parsed: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_reader(std::io::BufReader::new(reader))
            .map_err(|e| AppError::Other(format!("{}: {e}", source.display())))?;

    // Written beside the target and renamed: a half-written table would be
    // searched as though it were whole, and mapped memory does not notice.
    let temp = target.with_extension("tsv.part");
    {
        let mut out = BufWriter::new(File::create(&temp)?);
        for (word, value) in parsed {
            // Homographs arrive as a list of readings. They cannot be resolved
            // without context, so they are left out here entirely rather than
            // guessed at; `omographs` handles them separately.
            let Some(text) = value.as_str() else { continue };
            let Some(converted) = convert(text) else {
                continue;
            };
            writeln!(out, "{word}\t{converted}")?;
        }
        out.flush()?;
    }
    std::fs::rename(&temp, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny sorted table, standing in for the real one.
    fn table(lines: &[&str]) -> Vec<u8> {
        let mut sorted: Vec<&str> = lines.to_vec();
        sorted.sort();
        sorted.join("\n").into_bytes()
    }

    #[test]
    fn finds_every_entry() {
        let data = table(&["альфа\t0", "бета\t1", "гамма\t0", "дельта\t1", "омега\t2"]);
        for (word, expected) in
            [("альфа", "0"), ("бета", "1"), ("гамма", "0"), ("дельта", "1"), ("омега", "2")]
        {
            assert_eq!(lookup(&data, word), Some(expected), "не нашлось: {word}");
        }
    }

    #[test]
    fn reports_absence_rather_than_a_neighbour() {
        let data = table(&["альфа\t0", "гамма\t0", "омега\t2"]);
        // Before the first, between two, and after the last.
        assert_eq!(lookup(&data, "аа"), None);
        assert_eq!(lookup(&data, "бета"), None);
        assert_eq!(lookup(&data, "яяя"), None);
    }

    #[test]
    fn an_empty_table_is_not_a_crash() {
        assert_eq!(lookup(b"", "что угодно"), None);
    }

    #[test]
    fn counts_vowels_for_the_stress_index() {
        assert_eq!(vowel_count("министерство"), 4);
        assert_eq!(vowel_count("мгла"), 1);
        assert_eq!(vowel_count(""), 0);
    }

    #[test]
    fn splits_text_into_words_and_the_rest() {
        let parts: Vec<_> = split_words("Суд запретил: 19 тел.").collect();
        assert_eq!(parts[0], ("Суд", true));
        assert_eq!(parts[1], (" ", false));
        assert_eq!(parts[2], ("запретил", true));
        // A number is a token to be spoken, not a gap between words. Treating
        // it as a gap silently deleted every figure from the audio.
        assert!(parts.iter().any(|(p, w)| *p == "19" && *w), "число потерялось: {parts:?}");
    }

    #[test]
    fn case_is_carried_onto_the_replacement() {
        assert_eq!(match_case("Все", "всё"), "Всё");
        assert_eq!(match_case("все", "всё"), "всё");
    }

    /// Runs against the real three-million-entry dictionary when it has been
    /// installed, and quietly does nothing when it has not — so the suite stays
    /// runnable on a machine that never downloaded a voice, while the machine
    /// that did gets the check that matters.
    #[test]
    fn the_real_dictionary_fixes_what_espeak_gets_wrong() {
        let dir = match crate::paths::voices_dir() {
            Ok(d) if StressDictionary::is_installed(&d) => d,
            _ => return,
        };
        let dict = StressDictionary::open(&dir).expect("installed dictionary must open");

        // Every one of these espeak stresses incorrectly. The expected value is
        // which vowel carries the stress, counting from zero.
        for (word, vowel) in [
            ("министерство", 2usize),
            ("годовых", 2),
            ("ключевую", 2),
            ("людьми", 1),
            ("молодых", 2),
            ("новости", 0),
            ("статья", 1),
            ("источник", 1),
            ("рынок", 0),
            ("выросли", 0),
            ("университет", 4),
        ] {
            assert_eq!(dict.stress_of(word), Some(vowel), "неверное ударение: {word}");
        }

        // The word that went missing until ё was restored first.
        assert_eq!(dict.restore_yo("ученые"), "учёные");
        assert_eq!(dict.stress_of("ученые"), Some(1));

        // Absence must be reported as absence, not as a neighbour's answer.
        // Picking a "nonsense" word for this is a trap — the dictionary knows
        // абырвалг — so use something that cannot be a word form at all.
        assert_eq!(dict.stress_of("ъъъщщщ"), None);

        // The search itself, against the file rather than against a fixture:
        // take entries out of the middle of the real table and look each one
        // up. A binary search that lands on a neighbour passes every toy test
        // and fails here.
        let raw = std::fs::read_to_string(dir.join("ru-accents.tsv")).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        let step = lines.len() / 500;
        for line in lines.iter().step_by(step.max(1)) {
            let (word, expected) = line.split_once('\t').unwrap();
            assert_eq!(
                dict.stress_of(word).map(|n| n.to_string()).as_deref(),
                Some(expected),
                "поиск промахнулся на слове {word}"
            );
        }
    }

    #[test]
    fn hyphenated_words_stay_whole() {
        let parts: Vec<_> = split_words("что-то").collect();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0], ("что-то", true));
    }
}
