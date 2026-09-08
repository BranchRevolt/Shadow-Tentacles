// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Guards over the boundary between Rust and the window.
//!
//! The two sides are compiled separately and checked by nobody, so a field
//! dropped from a payload only fails in a running window. These tests read the
//! interface's own source and check every field, function, element and
//! translation key it touches against what defines them.
//!
//! A variable name is read as standing for one payload throughout, so two
//! payloads must not share one.

use std::collections::HashSet;

/// Names that are JavaScript's own, not fields of ours.
const BUILT_INS: &[&str] = &[
    "map",
    "filter",
    "length",
    "forEach",
    "join",
    "reduce",
    "find",
    "sort",
    "dataset",
    "textContent",
    "value",
    "checked",
    "paused",
    "play",
    "pause",
    "addEventListener",
    "hidden",
    "disabled",
    "style",
    "classList",
    "innerHTML",
    "querySelector",
    "querySelectorAll",
    "toFixed",
    "trim",
    "split",
    "slice",
    "indexOf",
    "startsWith",
    "includes",
    "some",
    "every",
    "push",
    "keys",
    "entries",
    "card",
    "audio",
    "button",
    "then",
    "catch",
    "finally",
    "message",
    "detail",
];

fn declared_fields(source: &str, name: &str) -> HashSet<String> {
    let Some(start) = source.find(&format!("pub struct {name} {{")) else {
        panic!("тип {name} не найден — тест устарел вместе с кодом");
    };
    let body = &source[start..];
    let end = body.find("\n}").expect("struct must close");
    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim().trim_start_matches("pub ");
            let (name, rest) = line.split_once(':')?;
            // Skip doc comments and attributes, which have no colon of this shape.
            (!name.is_empty()
                && !name.starts_with('/')
                && !name.starts_with('#')
                && rest.contains(|c: char| c.is_alphabetic()))
            .then(|| name.to_string())
        })
        .collect()
}

fn fields_used(script: &str, variable: &str) -> HashSet<String> {
    let mut used = HashSet::new();
    let needle = format!("{variable}.");
    let mut rest = script;
    while let Some(at) = rest.find(&needle) {
        // Only a whole word: `data.` must not match `metadata.`.
        let preceded_by_word = rest[..at]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '.');
        rest = &rest[at + needle.len()..];
        if preceded_by_word {
            continue;
        }
        let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
        if !name.is_empty() {
            used.insert(name);
        }
    }
    used
}

/// The window's script, which the page loads as two files into one scope.
fn window_script() -> String {
    format!("{}\n{}", include_str!("../ui/i18n.js"), include_str!("../ui/app.js"))
}

#[test]
fn every_field_the_window_reads_is_one_the_payload_carries() {
    let rust = include_str!("../src/app.rs");
    let script = &without_strings(&window_script());
    let built_ins: HashSet<&str> = BUILT_INS.iter().copied().collect();

    // The payload type, and the name the interface binds it to.
    for (payload, variable) in [
        ("Overview", "data"),
        ("CardDto", "c"),
        ("SourceDto", "s"),
        ("ModelDto", "m"),
        ("Speech", "speech"),
        ("VerdictDto", "v"),
        ("VoiceOption", "voice"),
        ("ServiceDto", "service"),
        // The source form, which is filled from this type and read back into
        // it: a field the window sets and Rust does not know is a field that
        // silently does nothing.
        ("NewSource", "spec"),
    ] {
        let declared = declared_fields(rust, payload);
        assert!(!declared.is_empty(), "{payload} выглядит пустым — тест разбирает не то");

        let missing: Vec<String> = fields_used(script, variable)
            .into_iter()
            .filter(|f| !declared.contains(f) && !built_ins.contains(f.as_str()))
            .collect();

        assert!(
            missing.is_empty(),
            "интерфейс читает у {payload} поля, которых нет: {}",
            missing.join(", ")
        );
    }
}

/// Names JavaScript, the DOM or Tauri provide.
const GLOBALS: &[&str] = &[
    "invoke",
    "listen",
    "Number",
    "String",
    "Boolean",
    "Array",
    "Object",
    "Math",
    "JSON",
    "Date",
    "Set",
    "Map",
    "Promise",
    "RegExp",
    "Error",
    "parseInt",
    "parseFloat",
    "isNaN",
    "setInterval",
    "clearInterval",
    "setTimeout",
    "clearTimeout",
    "FormData",
    "URL",
    "Audio",
    "encodeURIComponent",
    "decodeURIComponent",
    "console",
    "document",
    "window",
    "fetch",
    "Intl",
    "localStorage",
    "IntersectionObserver",
    // Keywords, which look like calls to a scanner this simple.
    "if",
    "for",
    "while",
    "switch",
    "catch",
    "return",
    "function",
    "typeof",
    "await",
    "async",
    "of",
    "in",
    "new",
    "else",
    "do",
    "case",
    "void",
    "delete",
    "yield",
];

/// The file with the contents of its quoted strings removed.
///
/// Prose has parentheses and full stops in it, which the scanners below would
/// otherwise read as calls and field accesses. Template literals are left
/// alone: the code inside `${…}` is real code.
fn without_strings(script: &str) -> String {
    let mut out = String::with_capacity(script.len());
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for c in script.chars() {
        match quote {
            Some(open) => {
                if escaped {
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == open {
                    quote = None;
                    out.push(c);
                } else if c == '\n' {
                    // An unterminated string cannot span a line; the scanner
                    // must not swallow the rest of the file over one stray quote.
                    quote = None;
                    out.push(c);
                }
            }
            None => {
                if c == '"' || c == '\'' {
                    quote = Some(c);
                }
                out.push(c);
            }
        }
    }
    out
}

/// The file with its whole-line comments removed.
///
/// Prose mentions calls: a comment explaining why `confirm()` is not used would
/// otherwise be read as using it. Only whole-line comments are dropped, so a
/// `https://` inside a string survives.
fn without_comments(script: &str) -> String {
    script
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Every identifier called as a function, in order of appearance.
fn called_names(script: &str) -> Vec<String> {
    let bytes: Vec<char> = script.chars().collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut names = Vec::new();

    for (i, c) in bytes.iter().enumerate() {
        if *c != '(' {
            continue;
        }
        let mut end = i;
        while end > 0 && bytes[end - 1].is_whitespace() {
            end -= 1;
        }
        let mut start = end;
        while start > 0 && is_word(bytes[start - 1]) {
            start -= 1;
        }
        if start == end {
            continue;
        }
        // A method call belongs to whatever object it is on; only bare calls
        // resolve against this file's own declarations.
        if start > 0 && (bytes[start - 1] == '.' || bytes[start - 1] == '?') {
            continue;
        }
        let name: String = bytes[start..end].iter().collect();
        if !name.starts_with(|c: char| c.is_ascii_digit()) {
            names.push(name);
        }
    }
    names
}

/// The parameter names in the first parenthesised list of `text`.
///
/// A function passed in and called by name is defined by its caller, not by
/// this file, so its parameters count as declarations here.
fn parameter_names(text: &str) -> Vec<String> {
    let Some(open) = text.find('(') else { return Vec::new() };
    let Some(close) = text[open..].find(')') else { return Vec::new() };
    text[open + 1..open + close]
        .split(',')
        .filter_map(|part| {
            let bare = part.trim().trim_start_matches("...");
            (!bare.is_empty() && bare.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '$'))
                .then(|| bare.to_string())
        })
        .collect()
}

/// Everything the file binds a name to: declarations, and destructured imports.
fn declared_names(script: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    let word = |s: &str| -> String {
        s.trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$')
            .collect()
    };

    for line in script.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("function ") {
            names.insert(word(rest));
            names.extend(parameter_names(rest));
        }
        if let Some(rest) = line.strip_prefix("async function ") {
            names.insert(word(rest));
            names.extend(parameter_names(rest));
        }
        // An arrow function's parameters, wherever it was written.
        if let Some(arrow) = line.find("=>") {
            names.extend(parameter_names(&line[..arrow]));
        }
        for keyword in ["const ", "let ", "var "] {
            let Some(rest) = line.strip_prefix(keyword) else {
                continue;
            };
            let rest = rest.trim_start();
            if let Some(inner) = rest.strip_prefix('{') {
                // `const { invoke } = window.__TAURI__.core`
                let Some(close) = inner.find('}') else {
                    continue;
                };
                for part in inner[..close].split(',') {
                    let bound = part.split(':').next_back().unwrap_or(part);
                    names.insert(word(bound));
                }
            } else {
                names.insert(word(rest));
            }
        }
    }
    names.remove("");
    names
}

/// Every function the window calls is one that exists.
///
/// A call to a deleted function throws only when that screen is opened, and
/// takes the rest of the screen with it.
#[test]
fn the_window_calls_nothing_it_has_not_defined() {
    let script = without_comments(&without_strings(&window_script()));
    let script = script.as_str();
    let declared = declared_names(script);
    let globals: HashSet<&str> = GLOBALS.iter().copied().collect();

    let missing: Vec<String> = called_names(script)
        .into_iter()
        .filter(|n| !declared.contains(n) && !globals.contains(n.as_str()))
        .collect();

    assert!(missing.is_empty(), "окно вызывает то, чего в нём нет: {}", missing.join(", "));
}

/// Every element the window reaches for is one the page declares.
///
/// `el("x")` on a missing id returns null, and the next property access ends
/// the script along with whatever it was about to draw.
#[test]
fn the_window_asks_only_for_elements_the_page_has() {
    let script = window_script();
    let script = script.as_str();
    let page = include_str!("../ui/index.html");

    let mut missing = Vec::new();
    let mut rest = script;
    while let Some(at) = rest.find("el(\"") {
        rest = &rest[at + 4..];
        let id: String = rest.chars().take_while(|c| *c != '"').collect();
        if !page.contains(&format!("id=\"{id}\"")) {
            missing.push(id);
        }
    }
    missing.sort();
    missing.dedup();

    assert!(
        missing.is_empty(),
        "окно ищет элементы, которых нет на странице: {}",
        missing.join(", ")
    );
}

/// Every key the window asks for is one the dictionaries can answer.
///
/// English is harvested from the page, so a `data-i18n` attribute is the
/// definition. Catches both a key nobody defines and a translation of a key
/// that no longer exists.
#[test]
fn every_word_the_window_asks_for_is_one_some_dictionary_has() {
    let script = without_comments(&window_script());
    let script = script.as_str();
    let page = include_str!("../ui/index.html");
    let dictionary = include_str!("../ui/i18n.js");

    let mut known: HashSet<String> = HashSet::new();
    // Defined by the page itself, in English.
    for attribute in ["data-i18n", "data-i18n-placeholder", "data-i18n-title", "data-i18n-aria"] {
        known.extend(quoted_after(page, &format!("{attribute}=\"")));
    }
    // Defined in the dictionaries. Every key is quoted and followed by a colon,
    // which no other quoted string in that file is.
    for line in dictionary.lines() {
        let line = line.trim();
        if !line.starts_with('"') {
            continue;
        }
        if let Some(key) = line[1..].split('"').next()
            && line[key.len() + 2..].trim_start().starts_with(':')
        {
            known.insert(key.to_string());
        }
    }
    assert!(known.len() > 100, "разбор словаря сломался: найдено {} ключей", known.len());

    // What the script asks for, whether by name or by prefix.
    let mut asked = first_argument_of(script, "t");
    asked.extend(first_argument_of(script, "labelOf"));

    let missing: Vec<&String> = asked
        .iter()
        .filter(|key| match key.contains('.') {
            // A whole key, asked for by name.
            true => !known.contains(*key),
            // A prefix, completed at runtime from a value Rust chose.
            false => !known.iter().any(|k| k.starts_with(&format!("{key}."))),
        })
        .collect();
    assert!(missing.is_empty(), "окно просит слова, которых нет ни в одном словаре: {missing:?}");

    // And nothing translated into a language for a key that has since gone.
    let orphans: Vec<&String> = known
        .iter()
        .filter(|key| key.starts_with("error.") || key.starts_with("job."))
        .filter(|key| !script.contains(key.as_str()) && !rust_uses(key))
        .collect();
    assert!(orphans.is_empty(), "словарь переводит то, чего никто не просит: {orphans:?}");
}

/// The strings that follow `needle`, up to the next quotation mark.
fn quoted_after<'a>(text: &'a str, needle: &'a str) -> impl Iterator<Item = String> + 'a {
    text.match_indices(needle)
        .map(move |(at, _)| text[at + needle.len()..].chars().take_while(|c| *c != '"').collect())
}

/// Every literal key handed to `name(...)`, as a whole word.
///
/// The whole-word part is the point: without it `t("` also matches the tail of
/// `start("cmd_collect")`, and the test starts demanding a translation for a
/// command name.
fn first_argument_of(text: &str, name: &str) -> Vec<String> {
    let needle = format!("{name}(\"");
    text.match_indices(&needle)
        .filter(|(at, _)| {
            text[..*at]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_alphanumeric() && c != '_' && c != '$' && c != '.')
        })
        .map(|(at, _)| {
            text[at + needle.len()..].chars().take_while(|c| *c != '"').collect::<String>()
        })
        .collect()
}

/// True when Rust names this code itself, which is how every job and every
/// refusal reaches the window.
fn rust_uses(key: &str) -> bool {
    let app = include_str!("../src/app.rs");
    let bare = key.strip_prefix("error.").unwrap_or(key);
    app.contains(&format!("\"{key}\"")) || app.contains(&format!("\"{bare}\""))
}

/// Every language answers every key.
///
/// A missing key falls back to English, which is right at runtime and
/// invisible: one label stays English until somebody notices.
#[test]
fn every_language_has_a_word_for_everything() {
    let page = include_str!("../ui/index.html");
    let dictionary = include_str!("../ui/i18n.js");

    let mut wanted: HashSet<String> = HashSet::new();
    for attribute in ["data-i18n", "data-i18n-placeholder", "data-i18n-title", "data-i18n-aria"] {
        wanted.extend(quoted_after(page, &format!("{attribute}=\"")));
    }
    wanted.extend(keys_of(dictionary, "STRINGS.en = {"));
    assert!(wanted.len() > 150, "разбор словаря сломался: {} ключей", wanted.len());

    for (locale, opening) in [
        ("ru", "STRINGS.ru = {"),
        ("de", "STRINGS.de = {"),
        ("fr", "STRINGS.fr = {"),
        ("es", "STRINGS.es = {"),
        ("pt-BR", "STRINGS[\"pt-BR\"] = {"),
        ("zh-CN", "STRINGS[\"zh-CN\"] = {"),
    ] {
        let has = keys_of(dictionary, opening);
        let mut missing: Vec<&String> = wanted.difference(&has).collect();
        missing.sort();
        assert!(missing.is_empty(), "в {locale} не переведено: {missing:?}");

        let mut extra: Vec<&String> = has.difference(&wanted).collect();
        extra.sort();
        assert!(extra.is_empty(), "в {locale} переведено то, чего нет: {extra:?}");
    }
}

/// The keys of one dictionary: the quoted names at the start of a line, from
/// `opening` to the closing brace.
fn keys_of(dictionary: &str, opening: &str) -> HashSet<String> {
    let start = dictionary
        .find(opening)
        .unwrap_or_else(|| panic!("словарь {opening} не найден — тест устарел вместе с кодом"));
    let body = &dictionary[start + opening.len()..];
    let end = body.find("\n};").expect("a dictionary must close");

    body[..end]
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let key = line.strip_prefix('"')?.split('"').next()?;
            line[key.len() + 2..].trim_start().starts_with(':').then(|| key.to_string())
        })
        .collect()
}

/// A setting reaches Rust inside a patch, and never as an argument.
///
/// A setting the window keeps its own copy of eventually disagrees with the one
/// on disk.
#[test]
fn the_window_never_asks_with_a_setting_of_its_own() {
    let rust = include_str!("../src/app.rs");
    let script = without_comments(include_str!("../ui/app.js"));
    let settings = declared_fields(rust, "SettingsPatch");
    assert!(!settings.is_empty(), "SettingsPatch выглядит пустым — тест разбирает не то");

    let mut offences = Vec::new();
    let mut rest = script.as_str();
    while let Some(at) = rest.find("invoke(\"") {
        rest = &rest[at + 8..];
        let command: String = rest.chars().take_while(|c| *c != '"').collect();
        // The calls allowed to carry settings, because carrying them is what
        // they are for. Saving, obviously — and checking, which happens
        // *before* saving: nobody wants to write a key down and only then find
        // out whether it was the right one, so a check is about what is on the
        // screen rather than what is on disk.
        if command == "cmd_save_settings" || command.starts_with("cmd_check_") {
            continue;
        }
        let args: String = rest.chars().take(200).take_while(|c| *c != ')').collect();
        for field in &settings {
            if args.contains(&format!("{field}:")) || args.contains(&format!("{field} ")) {
                offences.push(format!("{command} ← {field}"));
            }
        }
    }

    assert!(
        offences.is_empty(),
        "окно передаёт настройку как параметр вместо того, чтобы читать её из конфигурации: {}",
        offences.join(", ")
    );
}

/// Every form field the window reaches for is one the page declares.
///
/// `form.min_points.value = …` on a field that does not exist throws, and the
/// throw lands in a toast as a technical message about `undefined` — which is
/// how a renamed input turns into a form that half fills itself in.
#[test]
fn the_window_fills_only_fields_the_page_has() {
    let script = without_strings(&window_script());
    let page = include_str!("../ui/index.html");

    // Names the DOM and FormData provide, not fields of ours.
    const OWN: &[&str] = &[
        "get",
        "set",
        "has",
        "reset",
        "submit",
        "elements",
        "value",
        "checked",
        "scrollIntoView",
        "addEventListener",
        "querySelector",
        "querySelectorAll",
        "hidden",
        "dataset",
    ];
    let own: HashSet<&str> = OWN.iter().copied().collect();

    let missing: Vec<String> = fields_used(&script, "form")
        .into_iter()
        .filter(|f| !own.contains(f.as_str()))
        .filter(|f| !page.contains(&format!("name=\"{f}\"")))
        .collect();

    assert!(
        missing.is_empty(),
        "окно заполняет поля, которых нет на странице: {}",
        missing.join(", ")
    );
}
