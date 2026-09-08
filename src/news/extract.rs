// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Turning a fetched page into an article body.
//!
//! A chain of readability and site-specific selectors; what it produces goes to
//! `quality` rather than being trusted.

use chrono::{DateTime, Utc};
use dom_smoothie::{Config, Readability, TextMode};
use scraper::{Html, Selector};

use crate::config::Selectors;
use crate::error::{AppError, AppResult};
use crate::llm::Lang;

#[derive(Debug, Clone)]
pub struct Extracted {
    pub title: String,
    pub author: Option<String>,
    pub text: String,
    /// The markup of the extracted region only. The quality gate measures link
    /// density on this rather than on the page, which is the difference between
    /// asking "is this article a list of links?" and "is this a news site?".
    pub content_html: String,
    pub excerpt: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    pub lang: Option<Lang>,
}

/// Extract the article from `html`.
///
/// `selectors`, when the source provides them, take priority: a human who has
/// looked at the page beats a generic heuristic every time.
pub fn extract(html: &str, url: &str, selectors: Option<&Selectors>) -> AppResult<Extracted> {
    if let Some(sel) = selectors {
        if let Some(found) = extract_with_selectors(html, sel) {
            return Ok(found);
        }
        tracing::debug!("configured selectors matched nothing for {url}, falling back");
    }
    extract_readability(html, url)
}

fn extract_readability(html: &str, url: &str) -> AppResult<Extracted> {
    let cfg = Config {
        // Plain text: the model gets no benefit from markup, and tags in the
        // prompt only spend context.
        text_mode: TextMode::Formatted,
        ..Config::default()
    };

    let mut readability = Readability::new(html, Some(url), Some(cfg))
        .map_err(|e| AppError::Extract(format!("{url}: {e}")))?;

    let article = readability.parse().map_err(|e| AppError::Extract(format!("{url}: {e}")))?;

    let meta = readability.get_article_metadata(readability.parse_json_ld());

    Ok(Extracted {
        title: if article.title.is_empty() { meta.title.clone() } else { article.title.clone() },
        author: article.byline.clone().or(meta.byline.clone()),
        text: article.text_content.to_string(),
        content_html: article.content.to_string(),
        excerpt: article.excerpt.clone().or(meta.excerpt.clone()),
        published_at: meta.published_time.as_deref().and_then(parse_datetime),
        lang: meta.lang.as_deref().and_then(|l| l.parse().ok()),
    })
}

fn extract_with_selectors(html: &str, sel: &Selectors) -> Option<Extracted> {
    let body_selector = sel.body.as_deref()?;
    let doc = Html::parse_document(html);
    let body = Selector::parse(body_selector).ok()?;

    let strip: Vec<Selector> = sel.strip.iter().filter_map(|s| Selector::parse(s).ok()).collect();

    let element = doc.select(&body).next()?;

    // scraper has no DOM mutation, so removal is done by collecting the text of
    // the nodes to drop and subtracting it. Crude, but it only runs for
    // sources a human has already had to configure by hand.
    let mut text = element.text().collect::<Vec<_>>().join(" ");
    for s in &strip {
        for junk in doc.select(s) {
            let junk_text = junk.text().collect::<Vec<_>>().join(" ");
            if junk_text.trim().len() > 8 {
                text = text.replace(&junk_text, " ");
            }
        }
    }

    let text = normalize_whitespace(&text);
    if text.chars().count() < 64 {
        return None;
    }

    let title = Selector::parse("h1")
        .ok()
        .and_then(|h1| doc.select(&h1).next())
        .map(|e| normalize_whitespace(&e.text().collect::<String>()))
        .unwrap_or_default();

    Some(Extracted {
        title,
        author: None,
        content_html: element.html(),
        text,
        excerpt: None,
        published_at: None,
        lang: None,
    })
}

/// Collapse runs of whitespace, keeping paragraph breaks.
pub fn normalize_whitespace(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank_run = 0usize;
    for line in s.lines() {
        let trimmed = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if trimmed.is_empty() {
            blank_run += 1;
            continue;
        }
        if !out.is_empty() {
            out.push_str(if blank_run > 0 { "\n\n" } else { "\n" });
        }
        blank_run = 0;
        out.push_str(&trimmed);
    }
    out
}

/// Parse the many date shapes a news page might declare.
///
/// Publishers are inventive here, and an unreadable date is not a reason to
/// lose the article — the caller marks it uncertain instead.
pub fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.with_timezone(&Utc));
    }
    if let Ok(d) = DateTime::parse_from_rfc2822(s) {
        return Some(d.with_timezone(&Utc));
    }
    // RFC 2822 with a weekday that does not match the date. chrono rejects the
    // whole string over that mismatch, but feeds get the weekday wrong often
    // enough that dropping an otherwise-valid date would be the worse trade —
    // so retry without it, and normalize the alphabetic zones chrono declines.
    if let Some(d) = parse_rfc2822_loose(s) {
        return Some(d);
    }
    // Bare "2026-08-21 14:05:00" and "2026-08-21", assumed UTC, and the
    // separator-less "20260821T140500Z" that ISO 8601 also allows and chrono's
    // RFC 3339 reader declines, the dashes being missing.
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%dT%H:%M:%S", "%Y/%m/%d %H:%M:%S", "%Y%m%dT%H%M%SZ"] {
        if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(naive.and_utc());
        }
    }
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return date.and_hms_opt(0, 0, 0).map(|d| d.and_utc());
    }
    None
}

/// RFC 2822 without trusting the weekday, and with the alphabetic time zones
/// that appear in real feeds mapped onto numeric offsets.
fn parse_rfc2822_loose(s: &str) -> Option<DateTime<Utc>> {
    // Drop a leading "Thu, " if present — that part is not trusted.
    let body = match s.split_once(", ") {
        Some((head, rest)) if head.chars().count() == 3 => rest,
        _ => s,
    }
    .trim();

    // chrono parses numeric offsets only; these three cover what feeds emit.
    let normalized = body
        .replace(" GMT", " +0000")
        .replace(" UTC", " +0000")
        .replace(" UT", " +0000")
        .replace(" Z", " +0000");

    DateTime::parse_from_str(&normalized, "%d %b %Y %H:%M:%S %z")
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_keeps_paragraphs() {
        let text = normalize_whitespace("  a   b \n\n\n  c  \n d ");
        assert_eq!(text, "a b\n\nc\nd");
    }

    #[test]
    fn parses_common_date_shapes() {
        assert!(parse_datetime("2026-08-21T14:05:00+03:00").is_some());
        assert!(parse_datetime("Fri, 21 Aug 2026 14:05:00 +0300").is_some());
        // A wrong weekday must not cost the date: 21 Aug 2026 is a Friday.
        assert!(parse_datetime("Thu, 21 Aug 2026 14:05:00 GMT").is_some());
        assert!(parse_datetime("21 Aug 2026 14:05:00 UTC").is_some());
        assert!(parse_datetime("2026-08-21 14:05:00").is_some());
        assert!(parse_datetime("2026-08-21").is_some());
        // Some services stamp their articles this way, and everything from
        // them would be dated "unknown" if it could not be read.
        assert_eq!(parse_datetime("20260821T140500Z"), parse_datetime("2026-08-21T14:05:00Z"));
        assert!(parse_datetime("вчера").is_none());
    }

    #[test]
    fn extracts_body_from_a_plain_article() {
        let html = r#"<html><head><title>Ignored</title></head><body>
            <nav><a href="/a">Home</a><a href="/b">Sport</a></nav>
            <article><h1>Заголовок новости</h1>
            <p>Первый абзац достаточно длинный, чтобы пройти порог оценки читаемости и попасть в основной текст статьи целиком.</p>
            <p>Второй абзац тоже содержательный и длинный, он подтверждает, что извлечение забирает тело статьи, а не меню сайта.</p>
            </article>
            <aside class="related">Читайте также: другая новость</aside>
            </body></html>"#;
        let got = extract(html, "https://example.com/a", None).unwrap();
        assert!(got.text.contains("Первый абзац"), "text was: {}", got.text);
        assert!(!got.text.contains("Sport"), "navigation leaked into the body");
    }
}
