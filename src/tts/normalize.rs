// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Getting text into the shape the synthesizer reads correctly.
//!
//! espeak reads integers correctly on its own; this keeps them intact on the way
//! in. Russian typography separates thousands with a space, and a phonemizer
//! splitting on whitespace reads "2 900" as two numbers.

use std::sync::LazyLock;

use regex::Regex;

use crate::llm::Lang;

/// Prepare `text` to be spoken in `lang`.
pub fn normalize(text: &str, lang: Lang) -> String {
    // Currency first, while the symbol still sits beside its amount; then the
    // groups, decimals and everything else.
    let text = move_currency_after_amount(text, lang);
    let text = join_digit_groups(&text);
    let text = expand_decimals(&text, lang);
    expand_symbols(&text, lang)
}

/// Turn "2 900" and "150 000" into single numbers.
///
/// Applied repeatedly because a large figure has several separators: "1 234 567"
/// needs three passes, and one pass leaves "1 234567".
fn join_digit_groups(text: &str) -> String {
    static GROUPS: LazyLock<Regex> = LazyLock::new(|| {
        // A digit, a typographic space, three digits, then a non-digit, which
        // separates a thousands group from a year. The trailing character is
        // captured and put back because this crate has no look-around.
        Regex::new(r"(\d)[ \u{00a0}\u{202f}\u{2009}](\d{3})(\D|$)").expect("valid pattern")
    });

    let mut out = text.to_string();
    // A large figure has several separators: "1 234 567" needs more than one
    // pass, because each match consumes the character after the group.
    for _ in 0..4 {
        let next = GROUPS.replace_all(&out, "$1$2$3").into_owned();
        if next == out {
            break;
        }
        out = next;
    }
    out
}

/// Read "1,5" as a decimal rather than as two numbers with a pause.
fn expand_decimals(text: &str, lang: Lang) -> String {
    static DECIMAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(\d+)[,.](\d+)").expect("valid pattern"));

    DECIMAL
        .replace_all(text, |caps: &regex::Captures| {
            let whole = &caps[1];
            let fraction = &caps[2];
            match lang {
                Lang::Ru => {
                    // Russian says the denominator: "одна целая пять десятых".
                    let unit = match fraction.chars().count() {
                        1 => "десятых",
                        2 => "сотых",
                        3 => "тысячных",
                        _ => return caps[0].replace([',', '.'], " и "),
                    };
                    format!("{whole} целых {fraction} {unit}")
                }
                // The others read the separator as a word and the digits singly,
                // which is what espeak already does with a full stop.
                _ => format!("{whole}.{fraction}"),
            }
        })
        .into_owned()
}

/// Currency written before the amount, as English typography does it.
///
/// A plain replacement gives "долларов 5", which is not how the number is read
/// in any supported language — the unit follows the amount when spoken, whatever
/// side the symbol sits on when written. So the two are swapped.
fn move_currency_after_amount(text: &str, lang: Lang) -> String {
    static LEADING: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"([$€£₽])\s?(\d[\d\s.,]*\d|\d)").expect("valid pattern"));
    // The real is written "R$", and the rule below only knows single symbols:
    // left to it, "R$ 20" becomes "R20 dólares" with the R stranded. So the
    // two-character symbols go first.
    static REAL: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"R\$\s?(\d[\d\s.,]*\d|\d)").expect("valid pattern"));

    let text = match lang {
        Lang::Pt => REAL.replace_all(text, "$1 reais").into_owned(),
        _ => text.to_string(),
    };
    let text = text.as_str();

    LEADING
        .replace_all(text, |caps: &regex::Captures| {
            let word = currency_word(caps[1].chars().next().unwrap_or('$'), lang);
            format!("{} {word}", &caps[2])
        })
        .into_owned()
}

fn currency_word(symbol: char, lang: Lang) -> &'static str {
    match (symbol, lang) {
        ('$', Lang::Ru) => "долларов",
        ('€', Lang::Ru) => "евро",
        ('£', Lang::Ru) => "фунтов",
        ('₽', Lang::Ru) => "рублей",
        ('$', Lang::En) => "dollars",
        ('€', Lang::En) => "euros",
        ('£', Lang::En) => "pounds",
        ('$', Lang::De) => "Dollar",
        ('€', Lang::De) => "Euro",
        ('£', Lang::De) => "Pfund",
        ('$', Lang::Fr) => "dollars",
        ('€', Lang::Fr) => "euros",
        ('£', Lang::Fr) => "livres",
        ('$', Lang::Es) => "dólares",
        ('€', Lang::Es) => "euros",
        ('£', Lang::Es) => "libras",
        ('$', Lang::Pt) => "dólares",
        ('€', Lang::Pt) => "euros",
        ('£', Lang::Pt) => "libras",
        _ => "",
    }
}

/// Symbols and abbreviations, in the order that avoids one rule eating another.
fn expand_symbols(text: &str, lang: Lang) -> String {
    let pairs: &[(&str, &str)] = match lang {
        Lang::Ru => &[
            ("%", " процентов"),
            ("₽", " рублей"),
            ("$", " долларов"),
            ("€", " евро"),
            ("£", " фунтов"),
            ("№", "номер "),
            ("млрд", "миллиардов"),
            ("млн", "миллионов"),
            ("тыс.", "тысяч"),
            ("руб.", "рублей"),
            ("г.", "года"),
            ("гг.", "годов"),
            ("т.д.", "так далее"),
            ("т.п.", "тому подобное"),
            ("см.", "смотри"),
            ("ок.", "около"),
            ("им.", "имени"),
            ("&", " и "),
        ],
        Lang::En => &[
            ("%", " percent"),
            ("$", " dollars"),
            ("€", " euros"),
            ("£", " pounds"),
            ("&", " and "),
        ],
        Lang::De => &[
            ("%", " Prozent"),
            ("€", " Euro"),
            ("$", " Dollar"),
            ("Mrd.", "Milliarden"),
            ("Mio.", "Millionen"),
            ("&", " und "),
        ],
        Lang::Fr => &[("%", " pour cent"), ("€", " euros"), ("$", " dollars"), ("&", " et ")],
        Lang::Es => &[
            ("%", " por ciento"),
            ("€", " euros"),
            ("$", " dólares"),
            ("£", " libras"),
            ("EE. UU.", "Estados Unidos"),
            ("&", " y "),
        ],
        // "R$" before "$", or the first rule leaves the R behind.
        Lang::Pt => &[
            ("%", " por cento"),
            ("R$", " reais"),
            ("€", " euros"),
            ("$", " dólares"),
            ("£", " libras"),
            ("&", " e "),
        ],
    };

    let mut out = text.to_string();
    for (from, to) in pairs {
        if out.contains(from) {
            out = out.replace(from, to);
        }
    }
    collapse_spaces(&out)
}

fn collapse_spaces(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last_space = false;
    for c in text.chars() {
        let space = c == ' ';
        if space && last_space {
            continue;
        }
        last_space = space;
        out.push(c);
    }
    out.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_can_say_the_two_currencies_it_will_actually_meet() {
        // `currency_word` falls through to an empty string, so a language added
        // without one loses the unit silently: "5" where "5 dollars" was meant.
        for lang in Lang::ALL {
            for symbol in ['$', '€'] {
                assert!(
                    !currency_word(symbol, lang).is_empty(),
                    "{lang} не умеет произнести {symbol}"
                );
            }
        }
    }

    #[test]
    fn thousands_separated_by_a_space_become_one_number() {
        assert_eq!(join_digit_groups("2 900 человек"), "2900 человек");
        assert_eq!(join_digit_groups("150 000 рублей"), "150000 рублей");
        // A figure with several separators needs more than one pass.
        assert_eq!(join_digit_groups("1 234 567"), "1234567");
    }

    #[test]
    fn a_narrow_or_non_breaking_space_counts_too() {
        // Typesetting uses these, and a reader never sees the difference.
        assert_eq!(join_digit_groups("2\u{00a0}900"), "2900");
        assert_eq!(join_digit_groups("2\u{202f}900"), "2900");
    }

    #[test]
    fn a_number_followed_by_a_short_word_is_left_alone() {
        // "5 лет" is not a thousands group, and joining it would be nonsense.
        assert_eq!(join_digit_groups("5 лет"), "5 лет");
        // Nor is a group of four digits: "в 2026 году" must survive.
        assert_eq!(join_digit_groups("в 2026 году"), "в 2026 году");
    }

    #[test]
    fn two_separate_numbers_stay_separate() {
        // Not a thousands group: the second run is four digits long.
        assert_eq!(join_digit_groups("12 3456"), "12 3456");
    }

    #[test]
    fn decimals_are_read_as_decimals() {
        assert_eq!(expand_decimals("1,5 процента", Lang::Ru), "1 целых 5 десятых процента");
        assert_eq!(expand_decimals("0,25", Lang::Ru), "0 целых 25 сотых");
    }

    #[test]
    fn symbols_become_words() {
        assert_eq!(normalize("выросли на 8%", Lang::Ru), "выросли на 8 процентов");
        assert_eq!(normalize("5 млрд рублей", Lang::Ru), "5 миллиардов рублей");
        assert_eq!(normalize("$5", Lang::Ru), "5 долларов");
        assert_eq!(normalize("$1 200", Lang::Ru), "1200 долларов");
        assert_eq!(normalize("€30 million", Lang::En), "30 euros million");
        assert_eq!(normalize("up 8%", Lang::En), "up 8 percent");
    }

    #[test]
    fn the_whole_pipeline_holds_together() {
        assert_eq!(
            normalize("Продажи выросли на 12,5% до 1 250 000 ₽", Lang::Ru),
            "Продажи выросли на 12 целых 5 десятых процентов до 1250000 рублей"
        );
    }

    #[test]
    fn text_without_numbers_is_untouched() {
        let plain = "Обычная новость без единой цифры";
        assert_eq!(normalize(plain, Lang::Ru), plain);
    }
}
