// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Reading a publisher's sitemaps.
//!
//! Two shapes: a news map carries `news:title` and `news:publication_date` and
//! by specification reaches back two days; an archive map reaches years but
//! usually carries only addresses and dates. The caller is told which it got.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::error::AppResult;
use crate::news::fetch::{Fetched, Fetcher, decode_body};
use crate::news::filters::DateRange;
use crate::news::types::{Candidate, Metrics};
use crate::shutdown::Cancel;

/// How many lists of articles to read for one source in one run.
///
/// The backstop for an index that dates none of its children. Indexes do not
/// count against it, being kilobytes of names.
const MAX_LISTS: usize = 12;

/// The hard stop, counting everything fetched. A site that names itself in its
/// own index would otherwise walk for ever, and `seen` only catches the exact
/// repeat.
const MAX_FETCHES: usize = 40;

/// How many articles one source may contribute to a run.
///
/// A word that happens to name a section of a site matches hundreds of
/// addresses. The newest are taken and the caller is told how many were left,
/// a truncated answer being otherwise indistinguishable from a complete one.
const MAX_PER_SOURCE: usize = 300;

/// The largest map read. The BBC's archive files are seven megabytes;
/// anything an order of magnitude past that is not something to pull on a home
/// connection every run.
const MAX_MAP_BYTES: usize = 64 * 1024 * 1024;

/// One line of a map.
#[derive(Debug, Clone)]
pub struct Entry {
    pub url: String,
    pub date: Option<DateTime<Utc>>,
    /// The headline, when the map carried one. Only the news maps do.
    pub title: Option<String>,
}

/// What the maps of one source gave back.
pub struct Mapped {
    pub candidates: Vec<Candidate>,
    /// The oldest moment headlines were available for. Anything older in the
    /// window was matched by its address alone.
    pub titled_since: Option<DateTime<Utc>>,
    /// How many addresses in the window arrived without a headline.
    pub untitled: usize,
    /// How many matches were cut off the end because there were too many.
    /// Zero means what came back is everything the maps had.
    pub dropped: usize,
    /// How many maps were actually read. Zero means none was listed or none
    /// could be fetched, which is not the same as a map with nothing in the
    /// window.
    pub read: usize,
}

/// Everything the source published in the window, as far as its maps reach.
///
/// `matches` is applied here, against the headline or the address, rather than
/// after fetching: a three-day window on a large publisher is two thousand
/// articles.
pub async fn collect(
    maps: &[String],
    fetcher: &Fetcher,
    range: DateRange,
    matches: impl Fn(&Entry) -> bool,
    cancel: &Cancel,
) -> AppResult<Mapped> {
    let mut queue: Vec<String> = maps.to_vec();
    let mut seen: HashSet<String> = queue.iter().cloned().collect();
    let mut entries: Vec<Entry> = Vec::new();
    let mut lists = 0usize;
    let mut fetches = 0usize;
    let mut maps_read = 0usize;
    let mut untitled = 0usize;
    let mut titled_since: Option<DateTime<Utc>> = None;

    while let Some(url) = queue.pop() {
        if cancel.is_cancelled() {
            return Err(crate::error::AppError::Cancelled);
        }
        if lists >= MAX_LISTS || fetches >= MAX_FETCHES {
            tracing::debug!("stopping at {lists} list(s), {} left unopened", queue.len() + 1);
            break;
        }
        fetches += 1;

        let text = match read(&url, fetcher, cancel).await {
            Ok(text) => text,
            // One unreadable map is not the source failing. The others, and the
            // feed behind them, still have news.
            Err(e) => {
                tracing::debug!("{url}: {e}");
                continue;
            }
        };
        maps_read += 1;

        match parse(&text) {
            Map::Index(children) => {
                tracing::debug!("{url}: an index of {} map(s)", children.len());
                // Oldest last, so the newest maps are opened first: `pop` takes
                // from the end, and a window that runs out of budget should
                // spend it on the days the reader is most likely to want.
                let mut wanted: Vec<Child> =
                    children.into_iter().filter(|c| c.may_hold(range)).collect();
                wanted.sort_by(|a, b| a.newest.cmp(&b.newest));
                for child in wanted {
                    if seen.insert(child.url.clone()) {
                        queue.push(child.url);
                    }
                }
            }
            Map::Urls(found) => {
                lists += 1;
                let (all, mut in_window, mut kept) = (found.len(), 0usize, 0usize);
                for entry in found {
                    if !range.contains(entry.date) {
                        continue;
                    }
                    match (&entry.title, entry.date) {
                        (Some(_), Some(date)) => {
                            titled_since = Some(titled_since.map_or(date, |t| t.min(date)));
                        }
                        (None, _) => untitled += 1,
                        _ => {}
                    }
                    in_window += 1;
                    if matches(&entry) {
                        kept += 1;
                        entries.push(entry);
                    }
                }
                tracing::debug!("{url}: {all} listed, {in_window} in the window, {kept} wanted");
            }
        }
    }

    // The publisher lists in whatever order suits it; the reader wants the
    // newest first, and so does the cap that may cut this list short.
    entries.sort_by(|a, b| b.date.cmp(&a.date));
    entries.dedup_by(|a, b| a.url == b.url);
    let dropped = entries.len().saturating_sub(MAX_PER_SOURCE);
    entries.truncate(MAX_PER_SOURCE);

    Ok(Mapped {
        candidates: entries.into_iter().map(into_candidate).collect(),
        titled_since,
        untitled,
        dropped,
        read: maps_read,
    })
}

/// Where a site says its maps are: the `Sitemap:` lines of `robots.txt`,
/// falling back to the conventional `/sitemap.xml`.
pub async fn discover(site: &str, fetcher: &Fetcher, cancel: &Cancel) -> Vec<String> {
    let Ok(base) = url::Url::parse(site) else {
        return Vec::new();
    };
    let Ok(robots) = base.join("/robots.txt") else {
        return Vec::new();
    };

    let mut found = Vec::new();
    if let Ok(text) = read(robots.as_str(), fetcher, cancel).await {
        for line in text.lines() {
            let line = line.trim();
            // Case-insensitively, because Reuters writes it in capitals.
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("sitemap") {
                let value = value.trim();
                if !value.is_empty() {
                    found.push(value.to_string());
                }
            }
        }
    }
    if found.is_empty()
        && let Ok(guess) = base.join("/sitemap.xml")
    {
        found.push(guess.to_string());
    }
    found
}

/// Of the maps a site lists, the ones likely to be about news.
///
/// A large site also maps its recipes and its video archive; the file names are
/// the only clue available before downloading them.
pub fn worth_reading(maps: &[String]) -> Vec<String> {
    const NOT_NEWS: &[&str] = &[
        "video",
        "image",
        "picture",
        "food",
        "bitesize",
        "teach",
        "programme",
        "podcast",
        "weather",
        "recipe",
        "author",
        "topic",
        "tag",
        "profile",
    ];

    let promising: Vec<String> = maps
        .iter()
        .filter(|m| {
            let lower = m.to_ascii_lowercase();
            !NOT_NEWS.iter().any(|word| lower.contains(word))
        })
        .cloned()
        .collect();

    // Everything filtered out means the guesses were wrong for this site, and a
    // wrong guess must not turn into "this publisher has no maps".
    if promising.is_empty() { maps.to_vec() } else { promising }
}

/// A map named by an index, with whatever the index said about its age.
struct Child {
    url: String,
    newest: Option<DateTime<Utc>>,
}

impl Child {
    /// Could this map hold anything the window asks for?
    ///
    /// Two clues: a `<lastmod>` on the child in the index, or a month named in
    /// the child's own address (`sitemap-2026-09_1.xml`). With neither, the
    /// answer is yes, and `MAX_LISTS` bounds the cost of being wrong.
    fn may_hold(&self, range: DateRange) -> bool {
        let Some(since) = range.since else {
            return true;
        };
        if let Some(newest) = self.newest {
            return newest >= since;
        }
        match month_in(&self.url) {
            // A map named for a month holds that month; the window starts
            // after it only if it starts after the month ends.
            Some((year, month)) => {
                let ends = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
                chrono::NaiveDate::from_ymd_opt(ends.0, ends.1, 1)
                    .and_then(|d| d.and_hms_opt(0, 0, 0))
                    .map(|d| d.and_utc() >= since)
                    .unwrap_or(true)
            }
            None => true,
        }
    }
}

/// The year and month an address names, if it names one: `…-2026-09_1.xml`,
/// `…/2026/09/…`, `…_202609.xml`.
fn month_in(url: &str) -> Option<(i32, u32)> {
    let digits: Vec<char> = url.chars().collect();
    for start in 0..digits.len().saturating_sub(5) {
        // A year, then a month, with at most one character between them.
        if !digits[start..start + 4].iter().all(char::is_ascii_digit) {
            continue;
        }
        let year: i32 = digits[start..start + 4].iter().collect::<String>().parse().ok()?;
        // Wide, because publishers keep more than a lifetime: Spiegel's
        // archive starts in 1957, and a floor at the web's own age would leave
        // those maps undated and opened for nothing.
        if !(1900..=2100).contains(&year) {
            continue;
        }
        let mut i = start + 4;
        if i < digits.len() && !digits[i].is_ascii_digit() {
            i += 1;
        }
        if i + 2 > digits.len() || !digits[i..i + 2].iter().all(char::is_ascii_digit) {
            continue;
        }
        let month: u32 = digits[i..i + 2].iter().collect::<String>().parse().ok()?;
        if (1..=12).contains(&month) {
            return Some((year, month));
        }
    }
    None
}

enum Map {
    Index(Vec<Child>),
    Urls(Vec<Entry>),
}

/// Read one map, inflating it when the publisher served it compressed.
async fn read(url: &str, fetcher: &Fetcher, cancel: &Cancel) -> AppResult<String> {
    let Fetched { bytes, .. } = fetcher.get(url, cancel).await?;
    if bytes.len() > MAX_MAP_BYTES {
        return Err(crate::error::AppError::Other(format!(
            "{url}: {} bytes is more map than we will read",
            bytes.len()
        )));
    }
    // Many publishers serve `.xml.gz`, which reqwest does not unwrap:
    // it is a compressed file, not a compressed transfer.
    if bytes.starts_with(&[0x1f, 0x8b]) {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(&bytes[..])
            .take(MAX_MAP_BYTES as u64)
            .read_to_end(&mut out)
            .map_err(|e| crate::error::AppError::Other(format!("{url}: {e}")))?;
        return Ok(decode_body(&out));
    }
    Ok(decode_body(&bytes))
}

/// Read either shape of map.
///
/// Prefixes matter: a `<url>` carries its own `<loc>` and, beside it, an
/// `<image:loc>` pointing at a photograph. Unprefixed names are matched
/// exactly, prefixed ones only where a prefix is expected.
fn parse(xml: &str) -> Map {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut children: Vec<Child> = Vec::new();
    let mut entries: Vec<Entry> = Vec::new();

    // The element being read, and what has been collected for it so far.
    let mut in_entry = false;
    let mut in_child = false;
    let mut field: Option<Field> = None;
    let mut loc = String::new();
    let mut lastmod: Option<DateTime<Utc>> = None;
    let mut published: Option<DateTime<Utc>> = None;
    let mut title: Option<String> = None;

    loop {
        match reader.read_event() {
            Err(e) => {
                tracing::debug!("sitemap: {e}");
                break;
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(e)) => match e.name().as_ref() {
                b"url" => {
                    in_entry = true;
                    loc.clear();
                    (lastmod, published, title) = (None, None, None);
                }
                b"sitemap" => {
                    in_child = true;
                    loc.clear();
                    lastmod = None;
                }
                b"loc" => field = Some(Field::Loc),
                b"lastmod" => field = Some(Field::LastMod),
                name => field = news_field(name),
            },
            Ok(Event::Text(text)) => {
                let Some(what) = field else {
                    continue;
                };
                // `&amp;` and friends resolved: a headline with an ampersand
                // in it is common, and matching a word against the escape
                // would be matching against markup.
                let value = text.xml10_content().unwrap_or_default().trim().to_string();
                match what {
                    Field::Loc if loc.is_empty() => loc = value,
                    Field::Loc => {}
                    Field::LastMod => lastmod = crate::news::extract::parse_datetime(&value),
                    Field::Published => published = crate::news::extract::parse_datetime(&value),
                    Field::Title => title = (!value.is_empty()).then_some(value),
                }
            }
            Ok(Event::End(e)) => {
                field = None;
                match e.name().as_ref() {
                    b"url" if in_entry => {
                        in_entry = false;
                        if !loc.is_empty() {
                            entries.push(Entry {
                                url: std::mem::take(&mut loc),
                                date: published.or(lastmod),
                                title: title.take(),
                            });
                        }
                    }
                    b"sitemap" if in_child => {
                        in_child = false;
                        if !loc.is_empty() {
                            children.push(Child { url: std::mem::take(&mut loc), newest: lastmod });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if !children.is_empty() { Map::Index(children) } else { Map::Urls(entries) }
}

#[derive(Clone, Copy)]
enum Field {
    Loc,
    LastMod,
    Published,
    Title,
}

/// The two fields the news namespace contributes, recognized only when they
/// come with a prefix — `news:title` and not the `title` of anything else.
fn news_field(name: &[u8]) -> Option<Field> {
    let (prefix, local) = match name.iter().position(|b| *b == b':') {
        Some(at) => (&name[..at], &name[at + 1..]),
        None => return None,
    };
    if prefix != b"news" {
        return None;
    }
    match local {
        b"title" => Some(Field::Title),
        b"publication_date" => Some(Field::Published),
        _ => None,
    }
}

fn into_candidate(entry: Entry) -> Candidate {
    Candidate {
        url: entry.url,
        title: entry.title,
        published_at: entry.date,
        excerpt: None,
        discussion_url: None,
        metrics: Metrics::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_news_map_gives_headlines_and_dates() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
        <urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"
                xmlns:news="http://www.google.com/schemas/sitemap-news/0.9"
                xmlns:image="http://www.google.com/schemas/sitemap-image/1.1">
          <url>
            <loc>https://example.com/news/one</loc>
            <lastmod>2026-09-07T09:58:00Z</lastmod>
            <image:image><image:loc>https://img.example.com/photo.jpg</image:loc></image:image>
            <news:news>
              <news:publication>
                <news:name>Example</news:name>
                <news:language>en</news:language>
              </news:publication>
              <news:publication_date>2026-09-07T09:57:32Z</news:publication_date>
              <news:title>Everton hold United</news:title>
            </news:news>
          </url>
        </urlset>"#;

        let Map::Urls(entries) = parse(xml) else {
            panic!("a list of addresses, not an index");
        };
        assert_eq!(entries.len(), 1);
        // The photograph beside the article must not be taken for the article.
        assert_eq!(entries[0].url, "https://example.com/news/one");
        assert_eq!(entries[0].title.as_deref(), Some("Everton hold United"));
        // The publication date wins over the last-modified date: an article
        // edited today was still published when it was published.
        assert_eq!(entries[0].date, crate::news::extract::parse_datetime("2026-09-07T09:57:32Z"));
    }

    #[test]
    fn an_index_names_its_children_and_their_age() {
        let xml = r#"<?xml version="1.0"?>
        <sitemapindex xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">
          <sitemap>
            <loc>https://example.com/sitemaps/archive-1.xml</loc>
            <lastmod>2023-12-21T12:38:26Z</lastmod>
          </sitemap>
          <sitemap>
            <loc>https://example.com/sitemaps/archive-2.xml</loc>
            <lastmod>2026-09-07T09:54:40Z</lastmod>
          </sitemap>
        </sitemapindex>"#;

        let Map::Index(children) = parse(xml) else {
            panic!("an index, not a list of addresses");
        };
        assert_eq!(children.len(), 2);

        // A window that starts this year does not open a map that stopped
        // being written to in 2023.
        let range = DateRange::last_days(3);
        assert!(!children[0].may_hold(range));
        assert!(children[1].may_hold(range));
        // With no window there is nothing to rule anything out.
        assert!(children[0].may_hold(DateRange::default()));
    }

    #[test]
    fn a_map_named_for_a_month_is_dated_by_its_name() {
        // Spiegel's archive carries no lastmod, and names the month instead.
        let old = Child {
            url: "https://x.de/sitemaps/article/sitemap-1957-03_1.xml".into(),
            newest: None,
        };
        let now = Child {
            url: format!(
                "https://x.de/sitemaps/article/sitemap-{}_1.xml",
                Utc::now().format("%Y-%m")
            ),
            newest: None,
        };
        let range = DateRange::last_days(3);
        assert!(!old.may_hold(range));
        assert!(now.may_hold(range));

        assert_eq!(month_in("https://x.de/a/sitemap-1957-03_1.xml"), Some((1957, 3)));
        assert_eq!(month_in("https://x.de/2026/09/index.xml"), Some((2026, 9)));
        assert_eq!(month_in("https://x.de/sitemap.xml"), None);
    }

    #[test]
    fn the_maps_that_are_not_about_news_are_left_alone() {
        let all = vec![
            "https://www.bbc.co.uk/sitemaps/https-index-uk-news.xml".to_string(),
            "https://www.bbc.co.uk/food/sitemap.xml".to_string(),
            "https://www.bbc.co.uk/bitesize/sitemap/sitemapindex.xml".to_string(),
        ];
        assert_eq!(worth_reading(&all), vec![all[0].clone()]);

        // A site whose every map looks unpromising still gets read: the guess
        // is ours, and being wrong about it must not silence the publisher.
        let odd = vec!["https://x.com/video-sitemap.xml".to_string()];
        assert_eq!(worth_reading(&odd), odd);
    }
}
