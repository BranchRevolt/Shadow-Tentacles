// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Turning one article's text into a structured summary.
//!
//! An article that fits the context takes one pass straight to JSON; one that
//! does not is condensed into plain-text digests first, and only the final pass
//! has to satisfy the schema.

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::llm::chunker::split_chars;
use crate::llm::engine::{Engine, GenOptions};
use crate::llm::lang::{self, Lang};
use crate::shutdown::Cancel;

const ARTICLE_PROMPT: &str = include_str!("../../resources/prompts/article.md");
const CONDENSE_PROMPT: &str = include_str!("../../resources/prompts/condense.md");
const TRANSLATE_PROMPT: &str = include_str!("../../resources/prompts/translate.md");

/// Fingerprint of the prompts, computed at compile time.
///
/// Part of a summary's cache key, so editing a prompt produces new rows rather
/// than leaving output written under different instructions.
pub fn prompt_version(mode: Mode) -> String {
    format!("{:016x}-{}", PROMPT_FINGERPRINT, mode.as_str())
}

const PROMPT_FINGERPRINT: u64 = fnv1a(ARTICLE_PROMPT.as_bytes())
    ^ fnv1a(CONDENSE_PROMPT.as_bytes()).rotate_left(17)
    ^ fnv1a(TRANSLATE_PROMPT.as_bytes()).rotate_left(37);

const fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
        i += 1;
    }
    hash
}

/// How a summary of a foreign-language article reaches the reader's language.
///
/// `Direct` asks for it in the target language in one pass, which a 4B model
/// does badly across languages. `TwoPass` summarizes in the article's own
/// language and then translates the short result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Direct,
    TwoPass,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Direct => "direct",
            Mode::TwoPass => "2pass",
        }
    }
}

impl std::str::FromStr for Mode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "direct" | "1" | "one" => Ok(Mode::Direct),
            "2pass" | "two_pass" | "twopass" | "2" | "two" => Ok(Mode::TwoPass),
            other => Err(format!("unknown mode '{other}' (expected direct or 2pass)")),
        }
    }
}

/// The structured result the UI renders as a card.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArticleSummary {
    pub title: String,
    pub bullets: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default = "default_relevance")]
    pub relevance: f32,
}

fn default_relevance() -> f32 {
    0.5
}

/// What the model returned. `NotAnArticle` is a normal outcome, not an error:
/// it is the last and cheapest layer of the quality gate, and the caller
/// records it as a verdict on the article rather than a failure of the run.
#[derive(Debug, Clone)]
pub enum Outcome {
    Summary(ArticleSummary),
    NotAnArticle,
}

/// Summarize `text` into `target`, scoring relevance against `keywords`.
///
/// `progress` is called as (done, total) steps; the condense phase dominates
/// the wall clock on long articles, so each part is one step.
pub fn summarize_article(
    engine: &Engine,
    text: &str,
    target: Lang,
    keywords: &[String],
    mode: Mode,
    cancel: &Cancel,
    mut progress: impl FnMut(u32, u32),
) -> AppResult<Outcome> {
    // In two-pass mode the summary is written in the article's own language and
    // translated afterwards. When the article is already in the target language
    // there is nothing to translate, and the two modes coincide.
    let source = Lang::detect(text);
    let working = match (mode, source) {
        (Mode::TwoPass, Some(src)) if src != target => src,
        _ => target,
    };

    let system = build_system(ARTICLE_PROMPT, working, keywords);
    let opts = GenOptions::default();

    // Per-chunk data budget in REAL tokens: the context minus what will
    // generate, minus the system prompt, minus the ChatML framing. Sizing by
    // this instead of by characters is what stops a prompt from overflowing the
    // KV cache on dense (typically Russian) text.
    let system_tokens = engine.token_len(&system)?;
    let data_budget = (opts.ctx_size as usize)
        .saturating_sub(opts.max_new_tokens as usize)
        .saturating_sub(system_tokens)
        .saturating_sub(48)
        .max(256);

    let body = if engine.token_len(text)? <= data_budget {
        progress(0, 1);
        text.to_string()
    } else {
        condense_to_fit(engine, text, working, data_budget, cancel, &mut progress)?
    };

    let raw = engine.generate(&system, &body, &opts, cancel)?;
    let outcome = parse_outcome(&raw)?;

    let Outcome::Summary(mut summary) = outcome else {
        return Ok(Outcome::NotAnArticle);
    };

    if working != target {
        summary = translate(engine, summary, working, target, cancel)?;
    }

    // Prompt rules alone do not fully stop a 4B model from dropping a foreign
    // word into the output. Detect strays deterministically and run one focused
    // corrector pass, keeping the result only if it actually reduced the count —
    // the fix can then never make things worse.
    if target == Lang::Ru {
        summary = fix_strays(engine, summary, cancel);
    }

    Ok(Outcome::Summary(summary))
}

/// Translate a finished summary, field by field, in one call.
///
/// Numbered lines rather than JSON, a small model reproducing a list more
/// reliably than a schema. A wrong line count keeps the untranslated summary.
fn translate(
    engine: &Engine,
    summary: ArticleSummary,
    from: Lang,
    to: Lang,
    cancel: &Cancel,
) -> AppResult<ArticleSummary> {
    let mut lines: Vec<String> = Vec::with_capacity(1 + summary.bullets.len());
    lines.push(summary.title.clone());
    lines.extend(summary.bullets.iter().cloned());

    let numbered: String =
        lines.iter().enumerate().map(|(i, l)| format!("{}. {l}\n", i + 1)).collect();

    let system = format!(
        "{}\n\nSource language: {}. Target language: {}.\n\n{}",
        TRANSLATE_PROMPT.trim(),
        from.name(),
        to.name(),
        lang::directive(to),
    );

    // The output is about as long as the input, plus room for a language that
    // needs more tokens per word than the source did.
    let needed = engine.token_len(&numbered)?;
    let opts = GenOptions {
        temp: 0.2,
        repeat_penalty: 1.05,
        penalty_last_n: 64,
        max_new_tokens: (needed * 2 + 128) as i32,
        ..GenOptions::default()
    };

    let raw = engine.generate(&system, &numbered, &opts, cancel)?;
    let translated = parse_numbered(&raw, lines.len());

    let Some(mut translated) = translated else {
        tracing::warn!("translation returned the wrong number of lines, keeping {from}");
        return Ok(summary);
    };

    let title = translated.remove(0);
    Ok(ArticleSummary {
        title,
        bullets: translated,
        // Tags are single words used for filtering, not prose. Translating them
        // one model call at a time is not worth it; they stay as produced.
        tags: summary.tags,
        relevance: summary.relevance,
    })
}

/// Recover `expected` numbered lines from the model's reply, or None.
fn parse_numbered(raw: &str, expected: usize) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::with_capacity(expected);

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        // "12. text" / "12) text" / "12 - text"
        let Some((head, rest)) = line.split_once(['.', ')']) else {
            continue;
        };
        if head.trim().parse::<usize>().is_err() {
            continue;
        }
        let text = rest.trim().trim_start_matches('-').trim();
        if !text.is_empty() {
            out.push(text.to_string());
        }
    }

    (out.len() == expected).then_some(out)
}

/// Append the language directive and the reader's interests to a base prompt.
fn build_system(base: &str, target: Lang, keywords: &[String]) -> String {
    let mut s = String::with_capacity(base.len() + 512);
    s.push_str(base.trim());
    s.push_str("\n\n");
    if keywords.is_empty() {
        s.push_str("The reader stated no particular interests.\n\n");
    } else {
        s.push_str("The reader's interests: ");
        s.push_str(&keywords.join(", "));
        s.push_str("\n\n");
    }
    s.push_str(&lang::directive(target));
    s
}

/// Compress `text` until it fits `budget` tokens, in passes of plain-text digests.
///
/// Terminates because every digest is capped by `max_new_tokens`, which the
/// budget formula keeps well below `budget`, so each pass strictly shrinks the
/// input.
fn condense_to_fit(
    engine: &Engine,
    text: &str,
    target: Lang,
    budget: usize,
    cancel: &Cancel,
    progress: &mut impl FnMut(u32, u32),
) -> AppResult<String> {
    let system = build_system(CONDENSE_PROMPT, target, &[]);
    let opts = GenOptions { max_new_tokens: 512, ..GenOptions::default() };

    let mut current = text.to_string();
    let mut round = 0u32;

    loop {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        if engine.token_len(&current)? <= budget {
            return Ok(current);
        }

        let parts = chunk_by_tokens(engine, &current, budget, 200)?;
        // A single part that still exceeds the budget would loop forever;
        // chunk_by_tokens guarantees progress, but guard anyway.
        if parts.len() <= 1 {
            tracing::warn!("condense could not split further, truncating to budget");
            let ratio = budget as f64 / engine.token_len(&current)?.max(1) as f64;
            let keep = (current.chars().count() as f64 * ratio * 0.9) as usize;
            return Ok(current.chars().take(keep.max(256)).collect());
        }

        let total = parts.len() as u32;
        let mut digests = Vec::with_capacity(parts.len());
        for (i, part) in parts.iter().enumerate() {
            if cancel.is_cancelled() {
                return Err(AppError::Cancelled);
            }
            progress(i as u32, total);
            let user = format!("[Part {} of {}]\n\n{}", i + 1, total, part);
            digests.push(engine.generate(&system, &user, &opts, cancel)?);
        }
        progress(total, total);

        current = digests.join("\n\n");
        round += 1;
        tracing::debug!("condense round {round}: {} parts", parts.len());
    }
}

/// Split into chunks of at most `target_tokens` real tokens.
///
/// Measures the text's actual token density once and converts the token budget
/// into a character budget, with a safety factor so a denser-than-average
/// region still stays under budget.
fn chunk_by_tokens(
    engine: &Engine,
    text: &str,
    target_tokens: usize,
    overlap_tokens: usize,
) -> AppResult<Vec<String>> {
    let n_tokens = engine.token_len(text)?;
    let n_chars = text.chars().count();
    if n_tokens == 0 || n_chars == 0 || n_tokens <= target_tokens {
        return Ok(vec![text.to_string()]);
    }
    let chars_per_token = n_chars as f64 / n_tokens as f64;
    let chunk_chars = (target_tokens as f64 * chars_per_token * 0.85) as usize;
    let overlap_chars = (overlap_tokens as f64 * chars_per_token) as usize;
    Ok(split_chars(text, chunk_chars.max(1), overlap_chars))
}

/// Pull the JSON object out of whatever the model wrapped it in.
///
/// Instructed not to, small models still occasionally emit a code fence or a
/// sentence of preamble. Scanning for the first balanced `{…}` recovers those
/// instead of failing the article over formatting.
fn parse_outcome(raw: &str) -> AppResult<Outcome> {
    let json = extract_json(raw)
        .ok_or_else(|| AppError::Inference(format!("model returned no JSON: {raw:.200}")))?;

    // The not-an-article verdict has its own shape.
    #[derive(Deserialize)]
    struct MaybeError {
        error: Option<String>,
    }
    if let Ok(MaybeError { error: Some(_) }) = serde_json::from_str::<MaybeError>(json) {
        return Ok(Outcome::NotAnArticle);
    }

    let mut summary: ArticleSummary = serde_json::from_str(json)
        .map_err(|e| AppError::Inference(format!("malformed summary JSON: {e}: {json:.200}")))?;

    summary.title = summary.title.trim().to_string();
    summary.bullets.retain(|b| !b.trim().is_empty());
    summary.tags.retain(|t| !t.trim().is_empty());
    summary.tags.truncate(5);
    summary.relevance = summary.relevance.clamp(0.0, 1.0);

    if summary.title.is_empty() && summary.bullets.is_empty() {
        return Err(AppError::Inference("summary JSON was empty".into()));
    }
    Ok(Outcome::Summary(summary))
}

/// First balanced `{…}` run in `s`, ignoring braces inside string literals.
fn extract_json(s: &str) -> Option<&str> {
    let bytes = s.as_bytes();
    let start = s.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;

    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
            continue;
        }
        match c {
            b'"' => in_str = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&s[start..=i]);
                }
            }
            _ => {}
        }
    }
    None
}

/// One focused corrector pass over a Russian summary, replacing only stray
/// English words. Applied per field so the model rewrites short strings rather
/// than a whole document, which it does far more reliably.
fn fix_strays(engine: &Engine, mut summary: ArticleSummary, cancel: &Cancel) -> ArticleSummary {
    let joined = std::iter::once(&summary.title)
        .chain(summary.bullets.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ");
    let strays = lang::latin_words(&joined);
    if strays.is_empty() {
        return summary;
    }

    let system = format!(
        "You are a Russian proofreader. The text is Russian but contains English words: {}. \
         Replace each with its natural Russian equivalent, transliterating names of people, \
         places and brands into Cyrillic. Change NOTHING else — keep the meaning, wording and \
         structure identical. The result must contain no Latin letters. Output only the \
         corrected Russian text, with no quotes and no commentary.",
        strays.join(", ")
    );

    let fix = |text: &str| -> String {
        if lang::latin_words(text).is_empty() {
            return text.to_string();
        }
        let needed = engine.token_len(text).unwrap_or(text.chars().count() / 3);
        let opts = GenOptions {
            temp: 0.2,
            repeat_penalty: 1.05,
            penalty_last_n: 64,
            max_new_tokens: (needed + needed / 4 + 64) as i32,
            ..GenOptions::default()
        };
        match engine.generate(&system, text, &opts, cancel) {
            // Keep the correction only if it actually removed strays.
            Ok(fixed) if lang::latin_words(&fixed).len() < lang::latin_words(text).len() => fixed,
            _ => text.to_string(),
        }
    };

    summary.title = fix(&summary.title);
    summary.bullets = summary.bullets.iter().map(|b| fix(b)).collect();
    summary
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_version_separates_the_modes() {
        assert_eq!(prompt_version(Mode::Direct), prompt_version(Mode::Direct));
        assert_ne!(prompt_version(Mode::Direct), prompt_version(Mode::TwoPass));
        assert!(prompt_version(Mode::TwoPass).ends_with("-2pass"));
    }

    #[test]
    fn numbered_lines_round_trip() {
        let raw = "1. Первая строка\n2) Вторая строка\n";
        assert_eq!(
            parse_numbered(raw, 2).unwrap(),
            vec!["Первая строка".to_string(), "Вторая строка".to_string()]
        );
    }

    #[test]
    fn a_wrong_line_count_is_refused() {
        // Losing a line would silently pair each bullet with its neighbour's text.
        assert!(parse_numbered("1. Одна строка", 3).is_none());
        assert!(parse_numbered("1. a\n2. b\n3. c\n4. d", 3).is_none());
    }

    #[test]
    fn preamble_around_the_list_is_ignored() {
        let raw = "Here is the translation:\n\n1. Первая\n2. Вторая\n\nDone.";
        assert_eq!(parse_numbered(raw, 2).unwrap().len(), 2);
    }

    #[test]
    fn extracts_json_from_prose_and_fences() {
        let raw = "Here you go:\n```json\n{\"title\":\"a\",\"bullets\":[\"b\"]}\n```\nDone.";
        assert_eq!(extract_json(raw).unwrap(), "{\"title\":\"a\",\"bullets\":[\"b\"]}");
    }

    #[test]
    fn ignores_braces_inside_strings() {
        let raw = r#"{"title":"a } b","bullets":["c"]}"#;
        assert_eq!(extract_json(raw).unwrap(), raw);
    }

    #[test]
    fn recognizes_not_an_article() {
        let out = parse_outcome(r#"{"error": "not_an_article"}"#).unwrap();
        assert!(matches!(out, Outcome::NotAnArticle));
    }

    #[test]
    fn clamps_relevance_and_drops_blanks() {
        let out = parse_outcome(r#"{"title":"t","bullets":["a","  "],"tags":[],"relevance":7.0}"#)
            .unwrap();
        let Outcome::Summary(s) = out else { panic!("expected a summary") };
        assert_eq!(s.bullets.len(), 1);
        assert_eq!(s.relevance, 1.0);
    }

    #[test]
    fn missing_optional_fields_default() {
        let out = parse_outcome(r#"{"title":"t","bullets":["a"]}"#).unwrap();
        let Outcome::Summary(s) = out else { panic!("expected a summary") };
        assert_eq!(s.relevance, 0.5);
        assert!(s.tags.is_empty());
    }
}
