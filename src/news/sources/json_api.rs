// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Any service that answers with JSON.
//!
//! The address and the dotted paths to the values come from the configuration,
//! so a service becomes a source without code. Nested lists are reached the same
//! way: `items = "data.children"`, then `data.url` per item.

use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::config::{FieldMap, SourceSpec};
use crate::error::{AppError, AppResult};
use crate::news::fetch::Fetcher;
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
    let map = spec
        .map
        .as_ref()
        .ok_or_else(|| AppError::Config(format!("{}: json_api has no [map]", spec.label())))?;

    let url = super::fill(template, query, spec.min_points, range);
    tracing::debug!("{}: {url}", spec.label());
    let body = fetcher.get_text_with(&url, &spec.headers, cancel).await?;
    let root: Value = serde_json::from_str(&body)
        .map_err(|e| AppError::FeedParse(format!("{}: {e}", spec.label())))?;

    let candidates = extract(&root, map)?;
    tracing::info!("{}: {} item(s)", spec.label(), candidates.len());

    Ok(Collected { candidates, reach: Reach::Feed, titled_since: None, dropped: 0 })
}

/// Pull candidates out of a parsed response according to `map`.
pub fn extract(root: &Value, map: &FieldMap) -> AppResult<Vec<Candidate>> {
    let items = at(root, &map.items).ok_or_else(|| {
        AppError::FeedParse(format!(
            "nothing at '{}' — check the path to the list of items",
            if map.items.is_empty() { "the top level" } else { &map.items }
        ))
    })?;

    let array = items.as_array().ok_or_else(|| {
        AppError::FeedParse(format!(
            "'{}' is not a list; a service returns its items as an array",
            if map.items.is_empty() { "the top level" } else { &map.items }
        ))
    })?;

    Ok(array
        .iter()
        .filter_map(|item| {
            let discussion = text(item, map.discussion.as_deref())
                .or_else(|| map.discussion_template.as_deref().map(|t| interpolate(t, item)));

            // Some services publish posts of their own alongside links to
            // elsewhere — Ask HN and Show HN are the obvious case. Skipping
            // those loses a whole category of what the service carries, so a
            // source may say to keep them, pointed at their own page.
            let url = match text(item, Some(&map.url)) {
                Some(url) => url,
                None if map.url_from_discussion => discussion.clone()?,
                // Otherwise there is nothing to fetch, which is not an error —
                // the entry simply is not an article.
                None => return None,
            };

            Some(Candidate {
                url,
                title: text(item, map.title.as_deref()),
                published_at: map.published.as_deref().and_then(|p| timestamp(item, p)),
                excerpt: text(item, map.excerpt.as_deref()),
                discussion_url: discussion,
                metrics: Metrics {
                    score: map.score.as_deref().and_then(|p| number(item, p)),
                    comments: map.comments.as_deref().and_then(|p| number(item, p)),
                },
            })
        })
        .collect())
}

/// Fill `{path}` placeholders in a template from one item's own fields.
///
/// Services often hand out an id where a link would be more useful; this turns
/// `https://news.ycombinator.com/item?id={objectID}` into the address without
/// anyone writing code for that service.
fn interpolate(template: &str, item: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;

    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let Some(close) = rest[open..].find('}') else {
            // An unclosed brace is a typo, not a placeholder; keep it visible
            // rather than swallowing the remainder of the address.
            break;
        };
        let path = &rest[open + 1..open + close];
        out.push_str(&text(item, Some(path)).unwrap_or_default());
        rest = &rest[open + close + 1..];
    }
    out.push_str(rest);
    out
}

/// Walk a dotted path. An empty path returns the value itself.
fn at<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let path = path.trim();
    if path.is_empty() {
        return Some(value);
    }
    path.split('.').filter(|p| !p.is_empty()).try_fold(value, |node, key| match node {
        // A numeric segment indexes an array, so `items.0.url` works too.
        Value::Array(items) => key.parse::<usize>().ok().and_then(|i| items.get(i)),
        _ => node.get(key),
    })
}

fn text(item: &Value, path: Option<&str>) -> Option<String> {
    let value = at(item, path?)?;
    match value {
        Value::String(s) if !s.trim().is_empty() => Some(s.clone()),
        // Numbers appear where an id doubles as a link fragment.
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn number(item: &Value, path: &str) -> Option<i64> {
    match at(item, path)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A date, whichever of the two shapes a service uses.
///
/// Unix seconds and ISO 8601 are both common, sometimes in the same response,
/// and asking the user which one their service uses would be asking them to
/// know something they can just as well be spared.
fn timestamp(item: &Value, path: &str) -> Option<DateTime<Utc>> {
    match at(item, path)? {
        Value::Number(n) => n.as_i64().and_then(|secs| DateTime::<Utc>::from_timestamp(secs, 0)),
        Value::String(s) => crate::news::extract::parse_datetime(s)
            .or_else(|| s.trim().parse::<i64>().ok().and_then(|t| DateTime::from_timestamp(t, 0))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> FieldMap {
        FieldMap {
            items: String::new(),
            url: "url".into(),
            title: Some("title".into()),
            published: Some("created_at".into()),
            score: Some("score".into()),
            comments: None,
            discussion: Some("comments_url".into()),
            discussion_template: None,
            url_from_discussion: false,
            excerpt: None,
        }
    }

    #[test]
    fn reads_a_flat_array() {
        let json = serde_json::json!([
            {"url": "https://e.com/1", "title": "Один", "created_at": "2026-08-21T10:00:00Z",
             "score": 42, "comments_url": "https://e.com/1/c"},
            {"url": "https://e.com/2", "title": "Два", "created_at": 1755772800, "score": "7"}
        ]);
        let got = extract(&json, &map()).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].title.as_deref(), Some("Один"));
        assert_eq!(got[0].metrics.score, Some(42));
        assert_eq!(got[0].discussion_url.as_deref(), Some("https://e.com/1/c"));
        // A Unix timestamp and a numeric string are both understood.
        assert!(got[1].published_at.is_some());
        assert_eq!(got[1].metrics.score, Some(7));
    }

    #[test]
    fn walks_into_a_nested_list() {
        // The shape Reddit answers with.
        let json = serde_json::json!({
            "data": {"children": [
                {"data": {"url": "https://e.com/a", "title": "A", "ups": 5}}
            ]}
        });
        let nested = FieldMap {
            items: "data.children".into(),
            url: "data.url".into(),
            title: Some("data.title".into()),
            score: Some("data.ups".into()),
            ..map()
        };
        let got = extract(&json, &nested).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].url, "https://e.com/a");
        assert_eq!(got[0].metrics.score, Some(5));
    }

    #[test]
    fn an_id_becomes_a_discussion_link() {
        let json = serde_json::json!([{"objectID": "42", "title": "Ask HN: anything",
                                       "story_text": "the question"}]);
        let mut m = map();
        m.discussion = None;
        m.discussion_template = Some("https://news.ycombinator.com/item?id={objectID}".into());
        m.url_from_discussion = true;
        m.excerpt = Some("story_text".into());

        let got = extract(&json, &m).unwrap();
        assert_eq!(got.len(), 1, "a post with no outbound link must not vanish");
        assert_eq!(got[0].url, "https://news.ycombinator.com/item?id=42");
        assert_eq!(got[0].excerpt.as_deref(), Some("the question"));
    }

    #[test]
    fn without_that_option_a_linkless_item_is_still_skipped() {
        let json = serde_json::json!([{"objectID": "42", "title": "Ask HN"}]);
        let mut m = map();
        m.discussion_template = Some("https://e.com/{objectID}".into());
        assert!(extract(&json, &m).unwrap().is_empty());
    }

    #[test]
    fn a_template_without_placeholders_is_literal() {
        let json = serde_json::json!({"a": 1});
        assert_eq!(interpolate("https://e.com/fixed", &json), "https://e.com/fixed");
    }

    #[test]
    fn items_without_a_link_are_skipped_quietly() {
        let json = serde_json::json!([{"title": "нет ссылки"}, {"url": "https://e.com/1"}]);
        assert_eq!(extract(&json, &map()).unwrap().len(), 1);
    }

    #[test]
    fn a_wrong_path_explains_itself() {
        let json = serde_json::json!({"results": []});
        let mut m = map();
        m.items = "data.items".into();
        let error = extract(&json, &m).unwrap_err().to_string();
        assert!(error.contains("data.items"), "unhelpful message: {error}");
    }

    #[test]
    fn a_non_list_is_refused_with_a_reason() {
        let json = serde_json::json!({"data": {"items": {"url": "https://e.com"}}});
        let mut m = map();
        m.items = "data.items".into();
        assert!(extract(&json, &m).unwrap_err().to_string().contains("not a list"));
    }

    #[test]
    fn missing_optional_fields_are_simply_absent() {
        let json = serde_json::json!([{"url": "https://e.com/1"}]);
        let got = extract(&json, &map()).unwrap();
        assert_eq!(got[0].title, None);
        assert_eq!(got[0].metrics.score, None);
    }
}
