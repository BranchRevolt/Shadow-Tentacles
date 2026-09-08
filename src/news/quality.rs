// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Deciding whether what was extracted is really an article.
//!
//! Gates are ordered cheapest first. The last subtracts boilerplate: a fragment
//! seen in three or more of a domain's articles belongs to the site.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::llm::Lang;
use crate::news::types::Status;

/// Below this, this is a teaser, not an article.
const MIN_CHARS: usize = 300;
/// Above this share of link text, the extraction found a listing.
const MAX_LINK_DENSITY: f64 = 0.30;
/// Past this length, link density stops being evidence of anything: a long
/// body answers "is this a list rather than an article?" on its own, and a
/// release announcement full of links is not a listing page.
const LINK_DENSITY_APPLIES_BELOW: usize = 4_000;
/// A fragment seen in this many articles of one domain is site furniture.
pub const BOILERPLATE_THRESHOLD: i64 = 3;
/// This many timestamped entries mean a running feed.
const LIVEBLOG_ENTRIES: usize = 5;

/// Phrases that mean the page is asking for something rather than telling
/// something. Kept per language because a Russian paywall does not say "subscribe".
const JUNK_MARKERS: &[&str] = &[
    // en
    "enable javascript",
    "accept cookies",
    "subscribe to continue",
    "sign in to read",
    "you have reached your article limit",
    "read also",
    // ru
    "включите javascript",
    "принять cookie",
    "подпишитесь",
    "читайте также",
    "все права защищены",
    "материал доступен по подписке",
    // de
    "bitte aktivieren sie javascript",
    "cookies akzeptieren",
    "lesen sie auch",
    "abonnieren sie",
    // fr
    "activez javascript",
    "accepter les cookies",
    "lire aussi",
    "abonnez-vous",
    // es
    "activa javascript",
    "aceptar cookies",
    "lee también",
    "suscríbete",
    "contenido exclusivo para suscriptores",
    // pt
    "ative o javascript",
    "aceitar cookies",
    "leia também",
    "assine já",
    "conteúdo exclusivo para assinantes",
];

/// URL fragments the major publishers use for running coverage.
///
/// A live blog is a stream of updates, and summarizing it as one story
/// produces confident nonsense. Recognized by address, which is cheaper and
/// more certain than by content.
const LIVEBLOG_PATHS: &[&str] =
    &["/live/", "/liveblog", "/live-news", "/live-updates", "-live-blog", "/ticker/"];

/// Phrases that specifically indicate a paywall rather than generic junk.
const PAYWALL_MARKERS: &[&str] = &[
    "subscribe to continue",
    "sign in to read",
    "you have reached your article limit",
    "материал доступен по подписке",
    "abonnieren sie",
    "réservé aux abonnés",
    "contenido exclusivo para suscriptores",
    "solo para suscriptores",
    "conteúdo exclusivo para assinantes",
    "somente para assinantes",
];

#[derive(Debug, Clone)]
pub struct Verdict {
    pub status: Status,
    /// Why, in one short phrase — shown in the UI next to a doubtful article
    /// and logged for everything else.
    pub reason: Option<String>,
}

impl Verdict {
    fn ok() -> Verdict {
        Verdict { status: Status::Ok, reason: None }
    }
    fn reject(status: Status, reason: impl Into<String>) -> Verdict {
        Verdict { status, reason: Some(reason.into()) }
    }
}

/// Judge an extracted body.
///
/// `expected_lang` is what the source claims to publish in; a mismatch usually
/// means a consent page was fetched instead of the article. `html` must be the
/// markup of the extracted region, never the whole page, whose link ratio is
/// always high.
pub fn judge(text: &str, html: Option<&str>, url: &str, expected_lang: Option<Lang>) -> Verdict {
    if let Some(marker) = liveblog_marker(url) {
        return Verdict::reject(Status::NotAnArticle, format!("live coverage: '{marker}'"));
    }
    if timestamped_entries(text) >= LIVEBLOG_ENTRIES {
        return Verdict::reject(Status::NotAnArticle, "live coverage: timestamped entries");
    }

    let chars = text.chars().count();
    if chars < MIN_CHARS {
        return Verdict::reject(Status::LowQuality, format!("only {chars} characters"));
    }

    let lowered = text.to_lowercase();

    if let Some(marker) = PAYWALL_MARKERS.iter().find(|m| lowered.contains(*m)) {
        return Verdict::reject(Status::Paywalled, format!("paywall notice: '{marker}'"));
    }

    if chars < LINK_DENSITY_APPLIES_BELOW
        && let Some(html) = html
    {
        let density = link_density(html);
        if density > MAX_LINK_DENSITY {
            return Verdict::reject(
                Status::LowQuality,
                format!("link density {:.0}%", density * 100.0),
            );
        }
    }

    // A junk marker alone is weak evidence — a real article may well contain
    // "читайте также" in a caption. Require it in a short text, where the
    // marker makes up a meaningful share of the text.
    if chars < MIN_CHARS * 4
        && let Some(marker) = JUNK_MARKERS.iter().find(|m| lowered.contains(*m))
    {
        return Verdict::reject(Status::LowQuality, format!("boilerplate marker: '{marker}'"));
    }

    if let Some(expected) = expected_lang
        && let Some(detected) = Lang::detect(text)
        && detected != expected
    {
        return Verdict::reject(
            Status::LowQuality,
            format!("expected {expected}, detected {detected}"),
        );
    }

    Verdict::ok()
}

/// The live-coverage marker in this URL, if any.
fn liveblog_marker(url: &str) -> Option<&'static str> {
    let lowered = url.to_lowercase();
    LIVEBLOG_PATHS.iter().copied().find(|p| lowered.contains(p))
}

/// Lines that open with a clock time, the way a running feed stamps updates.
///
/// Counted at line starts only: a time inside a sentence is ordinary prose.
/// Language-independent, so it complements the URL test rather than repeating
/// it.
fn timestamped_entries(text: &str) -> usize {
    text.lines().filter(|line| starts_with_clock_time(line.trim_start())).count()
}

/// Does this line open with a wall-clock time?
///
/// `.` has to be accepted as a separator ("14.05 Uhr"), which makes a table of
/// percentages look like timestamps. So the hour and minute are range-checked
/// and what follows must not turn the number into something else.
fn starts_with_clock_time(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 5 || !(b[2] == b':' || b[2] == b'.') {
        return false;
    }
    let digits = |a: u8, c: u8| -> Option<u8> {
        (a.is_ascii_digit() && c.is_ascii_digit()).then(|| (a - b'0') * 10 + (c - b'0'))
    };
    let (Some(hour), Some(minute)) = (digits(b[0], b[1]), digits(b[3], b[4])) else {
        return false;
    };
    if hour > 23 || minute > 59 {
        return false;
    }
    // "09.30%" is a percentage, "09.301" is a version or a measurement.
    !matches!(b.get(5), Some(c) if c.is_ascii_digit() || *c == b'%' || *c == b':' || *c == b'.')
}

/// Share of the given markup's text that sits inside links.
fn link_density(html: &str) -> f64 {
    use scraper::{Html, Selector};

    let doc = Html::parse_document(html);
    let Ok(anchors) = Selector::parse("a") else {
        return 0.0;
    };

    let total: usize = doc.root_element().text().map(|t| t.trim().chars().count()).sum();
    if total == 0 {
        return 0.0;
    }
    let linked: usize = doc
        .select(&anchors)
        .map(|a| a.text().map(|t| t.trim().chars().count()).sum::<usize>())
        .sum();

    linked as f64 / total as f64
}

/// Split text into the units tracked for repetition: paragraphs, and long
/// lines within them.
pub fn fragments(text: &str) -> Vec<&str> {
    text.lines()
        .map(str::trim)
        // Short lines are dates, bylines and single words — too common to be
        // evidence of anything, and expensive to track.
        .filter(|l| l.chars().count() >= 40)
        .collect()
}

pub fn fragment_hash(fragment: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    // Normalize before hashing so trivial whitespace differences still match.
    fragment.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase().hash(&mut hasher);
    hasher.finish()
}

/// Remove fragments known to be this domain's furniture.
pub fn strip_boilerplate(text: &str, known: &[u64]) -> String {
    if known.is_empty() {
        return text.to_string();
    }
    text.lines()
        .filter(|line| {
            let trimmed = line.trim();
            trimmed.chars().count() < 40 || !known.contains(&fragment_hash(trimmed))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain article URL, for the tests that are not about the address.
    const ART: &str = "https://example.com/news/2026/08/21/story";

    fn long(text: &str) -> String {
        text.repeat(MIN_CHARS / text.chars().count().max(1) + 2)
    }

    #[test]
    fn short_text_is_rejected() {
        let v = judge("Слишком коротко.", None, ART, None);
        assert_eq!(v.status, Status::LowQuality);
    }

    #[test]
    fn a_real_body_passes() {
        let v = judge(&long("Обычный текст новости про события дня. "), None, ART, Some(Lang::Ru));
        assert_eq!(v.status, Status::Ok, "reason: {:?}", v.reason);
    }

    #[test]
    fn paywall_beats_generic_junk() {
        let text = long("Материал доступен по подписке. ");
        assert_eq!(judge(&text, None, ART, None).status, Status::Paywalled);
    }

    #[test]
    fn wrong_language_is_rejected() {
        let text = long("This is an English article body about the events of the day. ");
        let v = judge(&text, None, ART, Some(Lang::Ru));
        assert_eq!(v.status, Status::LowQuality);
        assert!(v.reason.unwrap().contains("detected en"));
    }

    #[test]
    fn a_link_list_is_rejected() {
        let html = "<div>".to_string()
            + &"<a href='/x'>Очень длинный заголовок ссылки в списке новостей</a>".repeat(20)
            + "<p>Немного текста</p></div>";
        let text = long("Очень длинный заголовок ссылки в списке новостей ");
        let v = judge(&text, Some(&html), ART, None);
        assert_eq!(v.status, Status::LowQuality);
        assert!(v.reason.unwrap().contains("link density"));
    }

    #[test]
    fn a_long_link_heavy_article_survives() {
        // From the wild: a release announcement, 15 000 characters, 64% of them
        // inside links to individual changes. A real article, and the density
        // test must not claim otherwise once the body is this long.
        let para = "Содержательный абзац с описанием изменения и его последствий. ";
        let mut html = String::from("<div>");
        for _ in 0..80 {
            html.push_str(&format!("<p>{para}<a href='/c'>{para}</a></p>"));
        }
        html.push_str("</div>");
        let text = para.repeat(160);
        assert!(text.chars().count() > LINK_DENSITY_APPLIES_BELOW);
        assert!(link_density(&html) > MAX_LINK_DENSITY);
        assert_eq!(judge(&text, Some(&html), ART, None).status, Status::Ok);
    }

    #[test]
    fn a_short_link_heavy_page_is_still_rejected() {
        // The France 24 video pages: a caption, and links everywhere else.
        let html = "<div><p>Courte légende de la vidéo.</p>".to_string()
            + &"<a href='/x'>Un autre titre de la liste des vidéos du jour</a>".repeat(12)
            + "</div>";
        let text = "Courte légende de la vidéo. ".repeat(14);
        assert!(text.chars().count() < LINK_DENSITY_APPLIES_BELOW);
        assert_eq!(judge(&text, Some(&html), ART, None).status, Status::LowQuality);
    }

    #[test]
    fn an_article_body_with_a_few_links_passes() {
        // What the gate must never reject: real prose carrying a couple of
        // in-text links. Measured over the whole page instead of this region,
        // the site's navigation would sink it.
        let body = "<div><p>".to_string()
            + &"Содержательный абзац новости с фактами и цифрами. ".repeat(30)
            + "</p><p>Подробности <a href='/x'>в отчёте</a> и <a href='/y'>здесь</a>.</p></div>";
        let text = "Содержательный абзац новости с фактами и цифрами. ".repeat(30);
        assert_eq!(judge(&text, Some(&body), ART, None).status, Status::Ok);
    }

    #[test]
    fn long_article_survives_an_incidental_marker() {
        // "Читайте также" in a caption must not sink a full-length article: the
        // marker gate deliberately applies only while the text is short enough
        // for the marker to be a meaningful share of it.
        let text = "Содержательный абзац новости с фактами и цифрами. ".repeat(40)
            + "\nЧитайте также: другая новость\n";
        assert!(text.chars().count() > MIN_CHARS * 4);
        assert_eq!(judge(&text, None, ART, None).status, Status::Ok);
    }

    #[test]
    fn a_stub_with_a_marker_is_rejected() {
        // The same marker in a short text is the whole point of the gate.
        let text = long("Читайте также: другая новость. ");
        assert!(text.chars().count() < MIN_CHARS * 4);
        assert_eq!(judge(&text, None, ART, None).status, Status::LowQuality);
    }

    #[test]
    fn boilerplate_is_stripped_once_known() {
        let furniture = "Подпишитесь на наш канал, чтобы не пропустить ничего важного";
        let text = format!("Первый абзац новости достаточной длины для учёта.\n{furniture}\n");
        let known = vec![fragment_hash(furniture)];
        let cleaned = strip_boilerplate(&text, &known);
        assert!(!cleaned.contains(furniture));
        assert!(cleaned.contains("Первый абзац"));
    }

    #[test]
    fn a_liveblog_url_is_not_an_article() {
        let text = long("Полноценный по длине текст, который иначе прошёл бы все ворота. ");
        let v = judge(&text, None, "https://theguardian.com/politics/live/2026/aug/21/x", None);
        assert_eq!(v.status, Status::NotAnArticle);
        assert!(v.reason.unwrap().contains("live coverage"));
    }

    #[test]
    fn a_timestamped_feed_is_caught_without_the_url() {
        // Publishers that do not put "live" in the path still stamp every entry.
        let mut text = String::new();
        for h in 9..16 {
            text.push_str(&format!("{h:02}:30 Очередное обновление ленты событий дня.\n"));
        }
        let v = judge(&text, None, ART, None);
        assert_eq!(v.status, Status::NotAnArticle);
    }

    #[test]
    fn percentages_are_not_timestamps() {
        // Found in the wild: a technical article with a table of percentages was
        // filed as live coverage because "24.55%" has the shape of a time.
        let text = "24.55%\n61.08%\n57.14%\n38.10%\n69.05%\n".to_string()
            + &long("Обычный текст технической статьи с измерениями и таблицами. ");
        assert_eq!(timestamped_entries(&text), 0);
        assert_eq!(judge(&text, None, ART, None).status, Status::Ok);
    }

    #[test]
    fn european_dot_times_still_count() {
        assert!(starts_with_clock_time("14.05 Uhr Die Lage bleibt unklar"));
        assert!(starts_with_clock_time("09:30 Первое обновление"));
        assert!(!starts_with_clock_time("99:99 не время"));
        assert!(!starts_with_clock_time("12.345 это версия"));
    }

    #[test]
    fn a_time_inside_prose_is_not_a_feed() {
        let text = long("Заседание началось в 14:05 и продолжалось до позднего вечера. ");
        assert_eq!(judge(&text, None, ART, None).status, Status::Ok);
    }

    #[test]
    fn a_normal_path_containing_alive_is_not_matched() {
        // "alive" must not trigger the "/live/" test.
        assert!(liveblog_marker("https://example.com/news/staying-alive-in-2026").is_none());
    }

    #[test]
    fn hash_ignores_whitespace_and_case() {
        assert_eq!(fragment_hash("Привет   мир"), fragment_hash("привет мир"));
    }
}
