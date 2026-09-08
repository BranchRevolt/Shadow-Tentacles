// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Orchestration: from a list of sources to summaries on disk.
//!
//! Collection is network-bound and runs wide; summarization runs one article at
//! a time against the resident model.

use std::time::Instant;

use futures_util::stream::{self, StreamExt};

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::llm::{Engine, Lang, Mode, Outcome, prompt_version, summarize_article};
use crate::news::dedup;
use crate::news::extract;
use crate::news::fetch::{Fetched, Fetcher, decode_body};
use crate::news::filters::{DateRange, KeywordFilter};
use crate::news::quality::{self, BOILERPLATE_THRESHOLD};
use crate::news::sources::{self, Reach};
use crate::news::types::{Article, Candidate, Status};
use crate::shutdown::{Cancel, Shutdown};
use crate::store::{Selection, Store};

/// How many pages are fetched and extracted at once. The per-host delay inside
/// `Fetcher` is what protects publishers; this only bounds memory here.
const FETCH_CONCURRENCY: usize = 6;

#[derive(Debug, Default)]
pub struct CollectReport {
    pub sources_polled: usize,
    /// Sources whose own map answered the whole window, headline by headline.
    /// The complete answer, and what the rest are measured against.
    pub sources_mapped: usize,
    /// Sources whose map covered the window but handed part of it over as bare
    /// addresses, so a word was only recognized where the address spelled it
    /// out. Named rather than counted, so a short result can be explained.
    pub sources_by_address: Vec<String>,
    /// Sources with no readable map, whose feed's day or two is all there
    /// was to have. Named for the same reason.
    pub sources_feed_only: Vec<String>,
    /// Sources that had more to give than one run will take, and how much was
    /// left behind. A cut answer that says nothing about being cut cannot be
    /// told apart from a complete one.
    pub sources_cut: Vec<(String, usize)>,
    /// Sources that could not be reached at all. Reported rather than logged:
    /// silently collecting from eight of nine looks like a slow news day.
    pub sources_failed: usize,
    pub candidates: usize,
    pub already_known: usize,
    pub out_of_range: usize,
    pub fetch_failed: usize,
    /// Turned away by the quality gates.
    pub rejected: usize,
    /// Fetched, readable, and not about anything the reader asked for. Counted
    /// apart from `rejected` because they are different answers to different
    /// questions, and a reader who set keywords deserves to see which number
    /// their words account for.
    pub off_topic: usize,
    pub stored: usize,
    pub duplicates: usize,
    pub pruned: i64,
}

#[derive(Debug, Default)]
pub struct SummarizeReport {
    pub attempted: usize,
    pub summarized: usize,
    pub not_an_article: usize,
    pub failed: usize,
    /// Articles a service would not take. Counted apart from `failed` because
    /// these leave the queue — the refusal is remembered — while a local model
    /// stumbling leaves the article where it was.
    pub refused: usize,
    /// Why the last failure failed. Shown beside the counts, which on their
    /// own leave the reason only in the log.
    pub last_failure: Option<AppError>,
    pub cancelled: bool,
}

/// What a collection run is busy with, for a caller that shows it.
///
/// Both stages are countable, and that is the point: a run takes minutes, and
/// a caller that cannot say how far along it is has nothing to draw.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// Asking each source what it has.
    Polling,
    /// Fetching and reading the pages those answers pointed at.
    Reading,
}

/// Poll every enabled source, fetch what is new, and store what survives.
///
/// `progress` is called with the stage and how many of how many are done.
pub async fn collect(
    config: &Config,
    store: &Store,
    fetcher: &Fetcher,
    range: DateRange,
    cancel: &Cancel,
    mut progress: impl FnMut(Stage, usize, usize),
) -> AppResult<CollectReport> {
    let mut report = CollectReport::default();
    let sources = config.enabled_sources().count();

    // --- gather candidates -------------------------------------------------
    let mut all: Vec<(String, Candidate)> = Vec::new();
    let terms = config.settings.search_terms();
    // Built before the sources are asked, not after: a map hands over the whole
    // window, and which of those lines is worth fetching has to be decided
    // there, on the headline, rather than by reading two thousand pages.
    let keywords = KeywordFilter::new(&config.settings.keywords, config.settings.output_lang);

    for spec in config.enabled_sources() {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        report.sources_polled += 1;
        progress(Stage::Polling, report.sources_polled, sources);

        let key = spec.url.clone().unwrap_or_else(|| spec.label());

        match sources::collect(spec, fetcher, range, &terms, &keywords, cancel).await {
            Ok(collected) => {
                store.note_source_reached(&key)?;
                match collected.reach {
                    Reach::Titles => report.sources_mapped += 1,
                    Reach::Addresses => report.sources_by_address.push(spec.label()),
                    Reach::Feed => report.sources_feed_only.push(spec.label()),
                }
                if collected.dropped > 0 {
                    report.sources_cut.push((spec.label(), collected.dropped));
                }
                for c in collected.candidates {
                    all.push((spec.label(), c));
                }
            }
            // One broken source must never take down a run: the other nine
            // still have news, and the user would rather see them. But it must
            // not vanish either — a feed that died a month ago looks exactly
            // like a quiet week from the outside.
            Err(e) => {
                tracing::warn!("{}: {e}", spec.label());
                report.sources_failed += 1;
                store.note_source_failure(&key, &e.to_string())?;
            }
        }
    }
    report.candidates = all.len();

    // --- cheap rejections, before any page is fetched ----------------------
    let mut todo: Vec<(String, Candidate)> = Vec::new();
    for (source, candidate) in all {
        if !range.contains(candidate.published_at) {
            report.out_of_range += 1;
            continue;
        }
        if store.has_article(&dedup::canonicalize(&candidate.url))? {
            report.already_known += 1;
            continue;
        }

        todo.push((source, candidate));
    }

    // --- fetch and extract, several at a time -------------------------------

    // Counted as they finish rather than as they start: several are in flight
    // at once, and "12 of 48" should mean twelve pages read, not twelve asked
    // for.
    let pages = todo.len();
    let mut read = 0usize;
    let results = stream::iter(todo)
        .map(|(source, candidate)| async move {
            let outcome = fetch_and_extract(fetcher, &candidate, cancel).await;
            (source, candidate, outcome)
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .inspect(|_| {
            read += 1;
            progress(Stage::Reading, read, pages);
        })
        .collect::<Vec<_>>()
        .await;

    // --- judge and store, one at a time ------------------------------------
    for (source, candidate, outcome) in results {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }

        let (content_html, mut article) = match outcome {
            Ok(pair) => pair,
            Err(e) => {
                tracing::debug!("{}: {e}", candidate.url);
                report.fetch_failed += 1;
                continue;
            }
        };

        // Subtract this domain's known furniture before judging, so a page
        // that is mostly menu is not credited with the menu's length.
        let domain = domain_of(&article.canonical_url);
        let known = store.known_boilerplate(&domain, BOILERPLATE_THRESHOLD)?;
        article.text = quality::strip_boilerplate(&article.text, &known);

        let expected = config.enabled_sources().find(|s| s.label() == source).and_then(|s| s.lang);
        let verdict =
            quality::judge(&article.text, Some(&content_html), &article.canonical_url, expected);
        article.status = verdict.status;

        if let Some(reason) = &verdict.reason {
            tracing::debug!("{} → {}: {reason}", article.canonical_url, verdict.status.as_str());
        }

        // Record every long fragment seen, so repetition across this domain's
        // articles becomes visible over time.
        for fragment in quality::fragments(&article.text) {
            store.note_fragment(&domain, quality::fragment_hash(fragment))?;
        }

        // Not stored at all: an article about something else is not a cheaper
        // article, it is one the reader did not ask for. This is the test that
        // keeps the database — and every minute of inference after it — about
        // the subject rather than about the feed.
        if article.status == Status::Ok && !keywords.matches(&article.text) {
            report.off_topic += 1;
            continue;
        }
        if article.status != Status::Ok {
            report.rejected += 1;
        }

        let hash = dedup::simhash(&article.text);
        let cluster = if hash != 0 {
            store.simhashes()?.into_iter().find(|(_, other)| dedup::is_near_duplicate(hash, *other))
        } else {
            None
        };

        let id = store.upsert_article(&article, &source, verdict.reason.as_deref())?;
        if hash != 0 {
            store.set_simhash(id, hash)?;
        }
        match cluster {
            Some((representative, _)) => {
                store.set_cluster(id, representative)?;
                report.duplicates += 1;
            }
            None => store.set_cluster(id, id)?,
        }
        report.stored += 1;
    }

    // Last, and only after a successful run: a canceled collection has not
    // earned the right to delete anything.
    if let Some(cutoff) = config.prune_before() {
        let pruned = store.prune(cutoff)?;
        report.pruned = pruned.articles;
        if pruned.articles > 0 {
            tracing::info!(
                "forgot {} article(s) older than {}, with {} summary/summaries and {} recording(s)",
                pruned.articles,
                cutoff.format("%Y-%m-%d"),
                pruned.summaries,
                pruned.recordings
            );
        }
    }

    Ok(report)
}

/// Fetch one candidate's page and extract the article from it.
async fn fetch_and_extract(
    fetcher: &Fetcher,
    candidate: &Candidate,
    cancel: &Cancel,
) -> AppResult<(String, Article)> {
    let Fetched { bytes, final_url } = fetcher.get(&candidate.url, cancel).await?;

    // For an address that redirects, the destination only becomes known here:
    // the chain has been followed and `final_url` is where it landed.
    let url = final_url;
    let page = decode_body(&bytes);
    let got = extract::extract(&page, &url, None)?;

    let title = if got.title.trim().is_empty() {
        candidate.title.clone().unwrap_or_default()
    } else {
        got.title.clone()
    };

    // Date precedence: what the feed said, then what the page declared. The
    // feed is usually the publisher's own record; page metadata is often the
    // template's idea of "now".
    let published_at = candidate.published_at.or(got.published_at);

    Ok((
        got.content_html.clone(),
        Article {
            canonical_url: dedup::canonicalize(&url),
            url,
            title,
            author: got.author,
            text: got.text,
            published_at,
            date_uncertain: published_at.is_none(),
            lang: got.lang,
            excerpt: got.excerpt.or_else(|| candidate.excerpt.clone()),
            discussion_url: candidate.discussion_url.clone(),
            metrics: candidate.metrics,
            status: Status::Ok,
        },
    ))
}

fn domain_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_lowercase()))
        .unwrap_or_default()
}

#[derive(Debug, Default)]
pub struct RecheckReport {
    pub examined: usize,
    pub changed: usize,
    pub summaries_dropped: usize,
}

/// Re-run the quality gates over everything already stored, so a tightened
/// rule also applies to what was collected before it.
///
/// The raw HTML is not kept, so the link-density test cannot run here; every
/// other gate works on text and address alone.
pub fn recheck(
    config: &Config,
    store: &Store,
    repromote: bool,
    cancel: &Cancel,
) -> AppResult<RecheckReport> {
    let mut report = RecheckReport::default();

    for article in store.all_for_recheck()? {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        report.examined += 1;

        let expected = article
            .source_label
            .as_deref()
            .and_then(|label| config.enabled_sources().find(|s| s.label() == label))
            .and_then(|s| s.lang)
            .or(article.lang);

        let verdict = quality::judge(&article.text, None, &article.canonical_url, expected);
        let previous = store.status_of(article.id)?;

        if previous == Some(verdict.status) {
            continue;
        }
        // Tighten freely; loosen only on request. Without the HTML the link
        // density test cannot run, so an automatic pass must not promote what
        // the full check once rejected — but when a gate itself turns out to be
        // wrong, as the timestamp heuristic did, its victims need a way back.
        if verdict.status == Status::Ok && !repromote {
            continue;
        }

        tracing::info!(
            "{} → {} ({})",
            article.canonical_url,
            verdict.status.as_str(),
            verdict.reason.as_deref().unwrap_or("no reason given")
        );
        store.set_status(article.id, verdict.status, verdict.reason.as_deref())?;
        report.changed += 1;

        // A summary of something that is not an article is worse than none.
        if verdict.status != Status::Ok {
            report.summaries_dropped += store.delete_summaries(article.id)?;
        }
    }

    Ok(report)
}

/// Summarize everything still pending, one article at a time.
///
/// "Pending" is bounded by the current selection. The engine keeps the model
/// loaded for the whole run and is unloaded by the caller.
// Every argument is a distinct type, so a swapped pair is a compile error.
#[allow(clippy::too_many_arguments)]
pub fn summarize_pending(
    store: &Store,
    engine: &Engine,
    model_id: &str,
    target: Lang,
    keywords: &[String],
    mode: Mode,
    batch: usize,
    selection: &Selection,
    cancel: &Cancel,
    mut on_article: impl FnMut(&str, usize, usize),
    mut on_batch: impl FnMut(),
) -> AppResult<SummarizeReport> {
    let mut report = SummarizeReport::default();
    let version = prompt_version(mode);

    // Everything there is to do, counted once at the start. The reader wants to
    // know how far through the whole of it they are — a run that said "30 of
    // 30" and stopped with two hundred left looked finished when it was not.
    let total = store.count_pending(target, &version, Some(model_id), selection)? as usize;
    let mut done = 0usize;

    // A batch at a time, and the next one starts by itself. Batches rather than
    // one long sweep because each one is written down and shown before the next
    // begins: a service that falls over on article ninety costs the reader
    // nothing that was already done.
    while !cancel.is_cancelled() {
        let pending = store.pending_summaries(target, model_id, &version, batch, selection)?;
        if pending.is_empty() {
            break;
        }
        // How many articles this batch takes out of the pending set — by being
        // written up, by being judged not an article, or by being refused. A
        // batch that takes out none would come back identical for ever, so it
        // is where the run stops.
        let resolved_before = resolved(&report);

        for article in pending {
            if cancel.is_cancelled() {
                report.cancelled = true;
                break;
            }
            // Every article is one unit of blocking work; the guard is what
            // lets a quit wait for it instead of abandoning it mid-write.
            let _guard = Shutdown::global().worker();

            report.attempted += 1;
            done += 1;
            // Clamped: an article that failed for a passing reason stays in the
            // queue and is offered again in a later batch, and a bar that read
            // "260 of 256" would be its own kind of lie.
            on_article(&article.title, done.min(total), total);

            let started = Instant::now();
            let result =
                summarize_article(engine, &article.text, target, keywords, mode, cancel, |_, _| {});

            match result {
                Ok(Outcome::Summary(summary)) => {
                    store.save_summary(article.id, target, model_id, &version, &summary)?;
                    report.summarized += 1;
                    tracing::info!(
                        "summarized in {:?}: {}",
                        started.elapsed(),
                        article.canonical_url
                    );
                }
                Ok(Outcome::NotAnArticle) => {
                    // The model is the last gate. Record the verdict so it
                    // never spend another minute on this page.
                    store.set_status(
                        article.id,
                        Status::NotAnArticle,
                        Some("the model said so"),
                    )?;
                    report.not_an_article += 1;
                }
                Err(AppError::Cancelled) => {
                    report.cancelled = true;
                    break;
                }
                // A refusal the next article would run into as well: a key that
                // was not accepted, an account with nothing in it. Two hundred
                // more attempts would collect the same answer and, on a metered
                // service, pay for the privilege.
                Err(e) if e.is_settled() => {
                    // The report goes nowhere from here — the error is what the
                    // caller shows, and it says more than a count would.
                    tracing::warn!("{}: {e}", article.canonical_url);
                    return Err(e);
                }
                Err(e) => {
                    tracing::warn!("{}: {e}", article.canonical_url);
                    // A service that refused this particular article will refuse
                    // it again tomorrow: remembered against the model that said
                    // no, so the run moves on and another model still gets its
                    // turn. Only refusals from a service — a local model failing
                    // is a failure of the machine, not a verdict on the article.
                    if e.code().starts_with("api_") {
                        store.note_refusal(
                            article.id,
                            model_id,
                            e.code(),
                            e.detail().as_deref(),
                        )?;
                        report.refused += 1;
                    }
                    report.failed += 1;
                    report.last_failure = Some(e);
                }
            }
        }

        if report.cancelled {
            break;
        }
        // Nothing left the queue, so the next batch would be this batch again.
        // That is a run failing on every article rather than working through
        // them, and going round for ever would only hide it.
        if resolved(&report) == resolved_before {
            tracing::warn!("a whole batch resolved nothing; stopping");
            break;
        }
        // The batch is written down. Show it before starting the next one, so
        // a long run fills the feed as it goes rather than at the very end.
        on_batch();
    }

    Ok(report)
}

/// Articles this run has taken out of the queue for good.
fn resolved(report: &SummarizeReport) -> usize {
    report.summarized + report.not_an_article + report.refused
}
