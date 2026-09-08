// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Feeds, and the listing-page fallback for sites without one.

use crate::config::SourceSpec;
use crate::error::{AppError, AppResult};
use crate::news::fetch::{Fetched, Fetcher, decode_body};
use crate::news::filters::DateRange;
use crate::news::types::{Candidate, Metrics};
use crate::shutdown::Cancel;

use super::{Collected, Reach};

pub async fn collect(
    spec: &SourceSpec,
    template: &str,
    fetcher: &Fetcher,
    range: DateRange,
    query: Option<&str>,
    cancel: &Cancel,
) -> AppResult<Collected> {
    // A feed address can be a search too — Hacker News is one — so the same
    // placeholders work here as anywhere else.
    let url = super::fill(template, query, spec.min_points, range);

    let Fetched { bytes, .. } = fetcher.get(&url, cancel).await?;
    let candidates = parse_feed(&bytes)?;
    tracing::info!("{}: {} item(s) in the feed", spec.label(), candidates.len());

    Ok(Collected { candidates, reach: Reach::Feed, titled_since: None, dropped: 0 })
}

/// Parse RSS, Atom or JSON Feed into candidates.
pub fn parse_feed(bytes: &[u8]) -> AppResult<Vec<Candidate>> {
    let feed = feed_rs::parser::parse(bytes).map_err(|e| AppError::FeedParse(e.to_string()))?;

    Ok(feed
        .entries
        .into_iter()
        .filter_map(|entry| {
            // An entry with no link can never be fetched.
            let url = entry.links.first().map(|l| l.href.clone())?;
            Some(Candidate {
                url,
                title: entry.title.map(|t| t.content),
                // `published` is what the publisher means; `updated` is a
                // fallback for feeds that only ever set the latter.
                published_at: entry.published.or(entry.updated),
                excerpt: entry
                    .summary
                    .map(|s| s.content)
                    .or_else(|| entry.content.and_then(|c| c.body)),
                discussion_url: None,
                metrics: Metrics::default(),
            })
        })
        .collect())
}

/// Scrape a listing page for article links.
pub async fn collect_html_list(
    spec: &SourceSpec,
    template: &str,
    fetcher: &Fetcher,
    range: DateRange,
    query: Option<&str>,
    cancel: &Cancel,
) -> AppResult<Collected> {
    use scraper::{Html, Selector};

    let url = super::fill(template, query, spec.min_points, range);
    let url = url.as_str();
    let selectors = spec.selectors.as_ref().ok_or_else(|| {
        AppError::Config(format!("{}: html_list needs [selectors]", spec.label()))
    })?;

    let Fetched { bytes, .. } = fetcher.get(url, cancel).await?;
    let html = decode_body(&bytes);
    let doc = Html::parse_document(&html);
    let selector = Selector::parse(&selectors.links)
        .map_err(|e| AppError::Config(format!("{}: bad links selector: {e}", spec.label())))?;

    let base = url::Url::parse(url).ok();
    let mut seen = std::collections::HashSet::new();
    let mut candidates = Vec::new();

    for element in doc.select(&selector) {
        let Some(href) = element.value().attr("href") else {
            continue;
        };
        // Listing pages are full of relative links; resolve against the page.
        let absolute = match &base {
            Some(b) => b.join(href).map(|u| u.to_string()).unwrap_or_else(|_| href.to_string()),
            None => href.to_string(),
        };
        if !seen.insert(absolute.clone()) {
            continue;
        }
        let title =
            element.text().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
        candidates.push(Candidate {
            url: absolute,
            title: (!title.is_empty()).then_some(title),
            // A listing rarely states a date; extraction will look for one.
            published_at: None,
            excerpt: None,
            discussion_url: None,
            metrics: Metrics::default(),
        });
    }

    tracing::info!("{}: {} link(s)", spec.label(), candidates.len());
    Ok(Collected { candidates, reach: Reach::Feed, titled_since: None, dropped: 0 })
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
    <rss version="2.0"><channel><title>Test</title>
      <item>
        <title>Первая новость</title>
        <link>https://example.com/1</link>
        <pubDate>Fri, 21 Aug 2026 14:05:00 +0300</pubDate>
        <description>Краткое изложение</description>
      </item>
      <item>
        <title>Без ссылки</title>
      </item>
    </channel></rss>"#;

    #[test]
    fn parses_rss_and_skips_linkless_items() {
        let items = parse_feed(RSS.as_bytes()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://example.com/1");
        assert_eq!(items[0].title.as_deref(), Some("Первая новость"));
        assert!(items[0].published_at.is_some());
        assert_eq!(items[0].excerpt.as_deref(), Some("Краткое изложение"));
    }

    #[test]
    fn parses_atom() {
        let atom = r#"<?xml version="1.0"?>
        <feed xmlns="http://www.w3.org/2005/Atom">
          <title>T</title>
          <entry>
            <title>Eintrag</title>
            <link href="https://example.de/a"/>
            <updated>2026-08-21T14:05:00Z</updated>
          </entry>
        </feed>"#;
        let items = parse_feed(atom.as_bytes()).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://example.de/a");
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse_feed(b"not a feed at all").is_err());
    }
}
