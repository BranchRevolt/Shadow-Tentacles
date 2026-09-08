// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Source adapters. Each returns `Candidate`s, whatever it read to find them.
//!
//! Adapters take no database handle, a SQLite connection not being `Sync`. Every
//! run asks the source the whole question again.

pub mod json_api;
pub mod rss;
pub mod sitemap;

use chrono::{DateTime, Utc};

use crate::config::{Kind, SourceSpec};
use crate::error::{AppError, AppResult};
use crate::news::fetch::Fetcher;
use crate::news::filters::{DateRange, KeywordFilter};
use crate::news::types::Candidate;
use crate::shutdown::Cancel;

/// Substitute the run's parameters into an address, so a source can push its
/// filtering onto the service.
///
/// Absent values are filled with a neutral one so the template stays valid;
/// `{query}` is the exception and is left empty.
pub fn fill(
    template: &str,
    query: Option<&str>,
    min_points: Option<i64>,
    range: DateRange,
) -> String {
    // Percent-encode what a query string cannot carry raw. Small enough to do
    // here, and it keeps URL parsing out of a path that runs per source.
    let encode = |v: &str| {
        v.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                b' ' => "+".to_string(),
                other => format!("%{other:02X}"),
            })
            .collect::<String>()
    };

    // Whole days back, for the services that count in them.
    let days = match range.since {
        Some(since) => ((chrono::Utc::now() - since).num_hours().max(1) + 23) / 24,
        None => 0,
    };

    let since = range.since.map(|d| d.timestamp()).unwrap_or(0);
    let until =
        range.until.map(|d| d.timestamp()).unwrap_or_else(|| chrono::Utc::now().timestamp());
    let as_date = |t: i64| {
        chrono::DateTime::from_timestamp(t, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default()
    };

    template
        .replace("{query}", &query.map(encode).unwrap_or_default())
        .replace("{min_points}", &min_points.unwrap_or(0).to_string())
        .replace("{since}", &since.to_string())
        .replace("{until}", &until.to_string())
        .replace("{since_date}", &as_date(since))
        .replace("{until_date}", &as_date(until))
        .replace("{days}", &days.to_string())
}

/// How completely a source answered for the window asked.
///
/// Reported rather than logged: a short result otherwise cannot be told apart
/// from a quiet week.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The publisher's own map, carrying a headline for everything in the
    /// window. This is the complete answer.
    Titles,
    /// The map covered the window, but part of it came back as addresses and
    /// dates with no headlines. A word is then only recognized where the
    /// address spells it out.
    Addresses,
    /// No readable map, so the feed's last day or two is all there was.
    Feed,
}

/// What a source handed back, and how far it reached to do it.
pub struct Collected {
    pub candidates: Vec<Candidate>,
    pub reach: Reach,
    /// The oldest moment headlines were available for, when a map answered.
    pub titled_since: Option<DateTime<Utc>>,
    /// How many matching articles were left behind because the source offered
    /// more of them than one run will read.
    pub dropped: usize,
}

impl Collected {
    fn feed(candidates: Vec<Candidate>) -> Collected {
        Collected { candidates, reach: Reach::Feed, titled_since: None, dropped: 0 }
    }
}

/// How many search words get a request in one run. A reader with
/// twenty keywords means all of them, but not at the price of twenty requests
/// per source per run — the rest are still applied to what comes back, where
/// they cost nothing.
const MAX_SEARCH_TERMS: usize = 8;

/// Everything one source published in the window.
///
/// The sitemap, the feed and the source's own search address are used together,
/// each reaching what the others do not. Keywords are matched against the map's
/// entries before anything is fetched.
pub async fn collect(
    spec: &SourceSpec,
    fetcher: &Fetcher,
    range: DateRange,
    terms: &[String],
    keywords: &KeywordFilter,
    cancel: &Cancel,
) -> AppResult<Collected> {
    let feed = spec
        .url
        .as_deref()
        .ok_or_else(|| AppError::Config(format!("{}: source has no url", spec.label())))?;

    // The feed first, so its versions of a story keep their place: it carries
    // the publisher's own summary, and a map carries nothing but the address.
    let mut collected = match one(spec, feed, fetcher, range, None, cancel).await {
        Ok(collected) => collected,
        // A dead feed is not a dead source any more. The map may still answer,
        // and if it does the reader never needs to know the feed was down.
        Err(e) => {
            tracing::debug!("{}: feed unreadable ({e})", spec.label());
            Collected::feed(Vec::new())
        }
    };
    let mut failure = None;

    let maps = maps_of(spec, feed, fetcher, cancel).await;
    if !maps.is_empty() {
        match sitemap::collect(&maps, fetcher, range, |e| worth_keeping(e, keywords), cancel).await
        {
            // A map that was never read leaves the source where it was: on its
            // feed. Reporting otherwise would call a publisher fully answered
            // because the host asked has no maps on it.
            Ok(mapped) if mapped.read > 0 => {
                let found = mapped.candidates.len();
                collected.titled_since = mapped.titled_since;
                collected.dropped = mapped.dropped;
                collected.reach =
                    if mapped.untitled == 0 { Reach::Titles } else { Reach::Addresses };
                let before = collected.candidates.len();
                merge(&mut collected.candidates, mapped.candidates);
                tracing::info!(
                    "{}: {before} in the feed, {found} in the map, {} in all",
                    spec.label(),
                    collected.candidates.len()
                );
            }
            Ok(_) => tracing::debug!("{}: no map answered", spec.label()),
            Err(e) => {
                tracing::warn!("{}: map unreadable ({e}), the feed still stands", spec.label());
                failure = Some(e);
            }
        }
    }

    if let Some(address) = search_address(spec, feed, terms) {
        let address = address.to_string();
        match ask(spec, &address, fetcher, range, terms, cancel).await {
            Ok(found) => merge(&mut collected.candidates, found),
            Err(e) => {
                tracing::warn!("{}: search failed ({e})", spec.label());
                failure = Some(e);
            }
        }
    }

    // Nothing from anywhere, and something went wrong on the way: that is a
    // source that failed, not a quiet one, and it must be named as such.
    match failure {
        Some(e) if collected.candidates.is_empty() => Err(e),
        _ => Ok(collected),
    }
}

/// Add what a second way in found, without disturbing what the first did.
fn merge(into: &mut Vec<Candidate>, found: Vec<Candidate>) {
    let mut seen: std::collections::HashSet<String> =
        into.iter().map(|c| crate::news::dedup::canonicalize(&c.url)).collect();
    for candidate in found {
        if seen.insert(crate::news::dedup::canonicalize(&candidate.url)) {
            into.push(candidate);
        }
    }
}

/// Is this line of the map one the reader asked for?
///
/// Matched against the headline where the map carried one, otherwise against
/// the address, which for most publishers contains the headline as a slug.
fn worth_keeping(entry: &sitemap::Entry, keywords: &KeywordFilter) -> bool {
    if keywords.is_empty() {
        return true;
    }
    match &entry.title {
        Some(title) => keywords.matches(title),
        None => keywords.matches(&words_in(&entry.url)),
    }
}

/// The words an address spells out, with its punctuation read as spaces.
fn words_in(url: &str) -> String {
    let path = url.split_once("//").map(|(_, rest)| rest).unwrap_or(url);
    path.chars().map(|c| if c.is_alphanumeric() { c } else { ' ' }).collect()
}

/// Where this source's maps are.
///
/// A written address is used as it stands; an empty string means the publisher
/// has none and stops the lookup on every run.
async fn maps_of(spec: &SourceSpec, feed: &str, fetcher: &Fetcher, cancel: &Cancel) -> Vec<String> {
    if let Some(written) = spec.sitemap_template() {
        return vec![written.to_string()];
    }
    if !spec.maps_wanted() {
        return Vec::new();
    }
    // A service answering with JSON is an aggregator, and its links are other
    // people's journalism. On Lobsters that was three maps and a hundred and
    // thirty thousand addresses for news its endpoint had already given.
    if spec.kind == Kind::JsonApi {
        return Vec::new();
    }
    // Likewise a source whose address takes a query: it is its own search, and
    // asking its API host for a sitemap of news serves no purpose.
    if feed.contains("{query}") {
        return Vec::new();
    }
    // The site, not the feed's own host. The BBC serves feeds from
    // `feeds.bbci.co.uk`, which has no articles on it and no map of any —
    // asking there gets nothing, and nothing looks exactly like a publisher
    // who keeps no map.
    let Some(site) = crate::config::publisher_site(feed) else {
        return Vec::new();
    };
    let home = format!("https://{site}/");
    sitemap::worth_reading(&sitemap::discover(&home, fetcher, cancel).await)
}

/// The address to put the reader's words to, if the source has one.
///
/// An address that already takes `{query}` is its own search. `None` means the
/// source cannot be asked, and its map answers instead.
pub fn search_address<'a>(
    spec: &'a SourceSpec,
    feed: &'a str,
    terms: &[String],
) -> Option<&'a str> {
    if terms.is_empty() {
        return None;
    }
    if feed.contains("{query}") {
        return Some(feed);
    }
    spec.search_template()
}

/// Put every word to a searchable address and pool the answers.
async fn ask(
    spec: &SourceSpec,
    search: &str,
    fetcher: &Fetcher,
    range: DateRange,
    terms: &[String],
    cancel: &Cancel,
) -> AppResult<Vec<Candidate>> {
    let mut candidates = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut last_error = None;

    for term in terms.iter().take(MAX_SEARCH_TERMS) {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        match one(spec, search, fetcher, range, Some(term), cancel).await {
            Ok(got) => {
                for candidate in got.candidates {
                    // The same story answers two of the reader's words often
                    // enough that fetching it twice would be routine.
                    if seen.insert(crate::news::dedup::canonicalize(&candidate.url)) {
                        candidates.push(candidate);
                    }
                }
            }
            // One word failing is not the source failing: the others still
            // have news, and reporting nothing would hide them.
            Err(e) => {
                tracing::warn!("{} ({term}): {e}", spec.label());
                last_error = Some(e);
            }
        }
    }

    match last_error {
        Some(e) if candidates.is_empty() => Err(e),
        _ => Ok(candidates),
    }
}

async fn one(
    spec: &SourceSpec,
    template: &str,
    fetcher: &Fetcher,
    range: DateRange,
    query: Option<&str>,
    cancel: &Cancel,
) -> AppResult<Collected> {
    match spec.kind {
        Kind::Rss => rss::collect(spec, template, fetcher, range, query, cancel).await,
        Kind::HtmlList => {
            rss::collect_html_list(spec, template, fetcher, range, query, cancel).await
        }
        Kind::JsonApi => json_api::collect(spec, template, fetcher, range, query, cancel).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled_and_encoded() {
        let range =
            DateRange { since: chrono::DateTime::from_timestamp(1_700_000_000, 0), until: None };
        let url = fill(
            "https://e.com/s?q={query}&p={min_points}&after={since}&from={since_date}",
            Some("local llm"),
            Some(100),
            range,
        );
        assert!(url.contains("q=local+llm"), "{url}");
        assert!(url.contains("p=100"), "{url}");
        assert!(url.contains("after=1700000000"), "{url}");
        assert!(url.contains("from=2023-11-14"), "{url}");
    }

    #[test]
    fn an_unset_window_stays_a_valid_query() {
        // "created_at_i>" is a syntax error at the far end; "…>0" is not, and
        // means what an unset start means.
        let url =
            fill("https://e.com/s?f=created_at_i%3E{since}", None, None, DateRange::default());
        assert!(url.ends_with("%3E0"), "{url}");
    }

    #[test]
    fn the_window_becomes_a_clause_or_nothing() {
        // A search phrase carries its own bound; a bare "when:" would be read
        // as the word.
        let url = fill("q={query}&d={days}", Some("ai"), None, DateRange::last_days(3));
        assert!(url.ends_with("&d=3"), "{url}");

        let hours =
            DateRange { since: Some(chrono::Utc::now() - chrono::Duration::hours(5)), until: None };
        assert!(fill("d={days}", None, None, hours).ends_with("d=1"), "part of a day is a day");
    }

    fn feed(url: &str, search: Option<&str>) -> SourceSpec {
        let mut spec: SourceSpec = toml::from_str(&format!("kind = \"rss\"\nurl = \"{url}\""))
            .expect("a source of two lines");
        spec.search_url = search.map(str::to_string);
        spec
    }

    #[test]
    fn words_go_to_the_address_that_can_answer_them() {
        let words = vec!["football".to_string()];
        let none: Vec<String> = Vec::new();

        // A plain feed with somewhere to search: the search address.
        let spec = feed("https://bbc.example/rss", Some("https://find.example/?q={query}"));
        let url = spec.url.clone().unwrap();
        assert_eq!(search_address(&spec, &url, &words), Some("https://find.example/?q={query}"));
        // With nothing to ask, the feed is read as it always was.
        assert_eq!(search_address(&spec, &url, &none), None);

        // A source that is already a search does not want a second address.
        let spec = feed("https://hn.example/?query={query}", Some("https://find.example/"));
        let url = spec.url.clone().unwrap();
        assert_eq!(search_address(&spec, &url, &words).unwrap(), url);

        // An address deliberately emptied means the same as absent for
        // collecting: the words are applied to the feed here.
        let spec = feed("https://blog.example/rss", Some(""));
        let url = spec.url.clone().unwrap();
        assert_eq!(search_address(&spec, &url, &words), None);
    }

    #[test]
    fn a_word_is_matched_against_the_headline_when_the_map_carried_one() {
        let words = KeywordFilter::new(&["football".to_string()], crate::llm::Lang::En);
        let titled = sitemap::Entry {
            url: "https://www.bbc.co.uk/news/articles/cddlm9lqpv0o".into(),
            date: None,
            title: Some("Everton hold United in football thriller".into()),
        };
        assert!(worth_keeping(&titled, &words));

        // The same address without a headline says nothing about football, and
        // the BBC's ids say nothing about anything.
        let opaque = sitemap::Entry { title: None, ..titled.clone() };
        assert!(!worth_keeping(&opaque, &words));

        // Most publishers write the headline into the address, and there the
        // word is recognized without fetching a thing.
        let slug = sitemap::Entry {
            url: "https://www.theguardian.com/football/2026/sep/07/everton-united".into(),
            date: None,
            title: None,
        };
        assert!(worth_keeping(&slug, &words));

        // The BBC's section is in the address even when the headline is not,
        // which is the one word such a source can still answer.
        let section = sitemap::Entry {
            url: "https://www.bbc.co.uk/sport/football/articles/c046pxeeqe6o".into(),
            date: None,
            title: None,
        };
        assert!(worth_keeping(&section, &words));

        // With no words at all, everything the window holds is wanted.
        let nothing = KeywordFilter::new(&[], crate::llm::Lang::En);
        assert!(worth_keeping(&opaque, &nothing));
    }

    #[test]
    fn a_url_without_placeholders_is_untouched() {
        let plain = "https://lobste.rs/newest.json";
        assert_eq!(fill(plain, Some("ignored"), Some(5), DateRange::last_days(3)), plain);
    }
}
