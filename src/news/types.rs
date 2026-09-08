// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Types that travel through the pipeline.
//!
//! A stage records a verdict rather than dropping an article.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::llm::Lang;

/// What a source offers before anything is fetched.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// Where the article itself lives. For an aggregator this is the external
    /// link, not the discussion page.
    pub url: String,
    pub title: Option<String>,
    pub published_at: Option<DateTime<Utc>>,
    /// Summary or excerpt the feed carried. Worth keeping: when the page is
    /// paywalled this is all the reader will ever get.
    pub excerpt: Option<String>,
    /// Where the reader can discuss it — the Hacker News thread, say.
    pub discussion_url: Option<String>,
    /// Popularity signal from an aggregator: points, upvotes, comment count.
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub score: Option<i64>,
    pub comments: Option<i64>,
}

/// An article after fetching and extraction, with the verdict of the gates.
#[derive(Debug, Clone)]
pub struct Article {
    pub url: String,
    pub canonical_url: String,
    pub title: String,
    pub author: Option<String>,
    pub text: String,
    pub published_at: Option<DateTime<Utc>>,
    /// True when no source gave a trustworthy date. The article still shows,
    /// carrying the doubt, rather than vanishing from a date-filtered view.
    pub date_uncertain: bool,
    pub lang: Option<Lang>,
    pub excerpt: Option<String>,
    pub discussion_url: Option<String>,
    pub metrics: Metrics,
    pub status: Status,
}

/// The verdict of the quality gates. Not an error type — these are outcomes the pipeline
/// expect and display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// Extraction produced a plausible article body.
    Ok,
    /// Text was extracted, but it reads as navigation, boilerplate or a stub.
    LowQuality,
    /// The publisher gave a teaser and asked for money.
    Paywalled,
    /// Fetch or extraction failed outright.
    Failed,
    /// The model looked at it and said it is not a news article.
    NotAnArticle,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::LowQuality => "low_quality",
            Status::Paywalled => "paywalled",
            Status::Failed => "failed",
            Status::NotAnArticle => "not_an_article",
        }
    }

    /// The verdict written in the database, read back.
    ///
    /// `None` for anything else, what a row written by a newer version
    /// of this program looks like to an older one.
    pub fn parse(s: &str) -> Option<Status> {
        Some(match s {
            "ok" => Status::Ok,
            "low_quality" => Status::LowQuality,
            "paywalled" => Status::Paywalled,
            "failed" => Status::Failed,
            "not_an_article" => Status::NotAnArticle,
            _ => return None,
        })
    }

    /// Whether it is worth spending a minute of inference on this article.
    pub fn worth_summarizing(self) -> bool {
        matches!(self, Status::Ok)
    }
}
