// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! SQLite store: every pipeline stage writes its result here.
//!
//! `summaries` is unique on (article, language, model, prompt version), so a new
//! model or prompt adds a row rather than replacing one.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};

use crate::config::Config;
use crate::error::AppResult;
use crate::llm::{ArticleSummary, Lang};
use crate::news::filters::KeywordFilter;
use crate::news::types::{Article, Status};

pub struct Store {
    conn: Connection,
}

/// What the reader has currently selected: which sources, which window, which
/// words.
///
/// A filter over the store, not a deletion: a source switched back on brings
/// its articles and their summaries straight back.
pub struct Selection {
    /// Labels of the sources in play. `None` means every source, which is what
    /// maintenance work over the whole store wants.
    sources: Option<Vec<String>>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
    keywords: KeywordFilter,
    /// What the reader typed into the box over the feed.
    ///
    /// Applied on top of `keywords` rather than instead of them: searching the
    /// feed looks inside it, it does not widen it.
    narrowing: Option<KeywordFilter>,
}

impl Selection {
    /// Everything held, unfiltered.
    pub fn everything() -> Selection {
        Selection {
            sources: None,
            since: None,
            until: None,
            keywords: KeywordFilter::new(&[], Lang::Ru),
            narrowing: None,
        }
    }

    pub fn from_config(config: &Config) -> Selection {
        let range = config.date_range();
        Selection {
            sources: Some(config.enabled_sources().map(|s| s.label()).collect()),
            since: range.since,
            until: range.until,
            keywords: KeywordFilter::new(&config.settings.keywords, config.settings.output_lang),
            narrowing: None,
        }
    }

    /// The same selection, narrowed by a search term. Matched on stems, as
    /// keywords are everywhere else here.
    pub fn searching(mut self, query: &str, lang: Lang) -> Selection {
        let query = query.trim();
        if !query.is_empty() {
            self.narrowing = Some(KeywordFilter::new(&[query.to_string()], lang));
        }
        self
    }

    /// The `WHERE` fragment for the articles table under `alias`, and the values
    /// that go with it, in the order they appear.
    fn clause(&self, alias: &str) -> (String, Vec<Value>) {
        let mut sql = String::new();
        let mut args = Vec::new();

        if let Some(labels) = &self.sources {
            if labels.is_empty() {
                // No source selected means no news, not all news.
                sql.push_str(" AND 0");
            } else {
                let holes = vec!["?"; labels.len()].join(",");
                sql.push_str(&format!(" AND {alias}.source_label IN ({holes})"));
                args.extend(labels.iter().cloned().map(Value::from));
            }
        }
        // An unknown date passes, exactly as it does during collection: dates
        // are missing often enough that excluding those would quietly hide real
        // articles, and the card says the date is uncertain.
        if let Some(since) = self.since {
            sql.push_str(&format!(
                " AND ({alias}.published_at IS NULL OR {alias}.published_at >= ?)"
            ));
            args.push(Value::from(since.to_rfc3339()));
        }
        if let Some(until) = self.until {
            sql.push_str(&format!(
                " AND ({alias}.published_at IS NULL OR {alias}.published_at <= ?)"
            ));
            args.push(Value::from(until.to_rfc3339()));
        }
        (sql, args)
    }

    /// Keywords are applied here rather than in SQL because they are matched on
    /// stems: FTS5 tokenises, it does not stem, so `санкции` would not find
    /// `санкциями` — the case the filter exists for.
    fn allows(&self, text: &str) -> bool {
        self.keywords.matches(text)
    }

    /// Does this card answer what the reader typed over the feed?
    ///
    /// Matched against the summary as well as the article: the summary is in
    /// the reader's language and the article often is not, so a word visible on
    /// the card has to find it.
    fn narrows_to(&self, card: &Card, text: &str) -> bool {
        let Some(narrowing) = &self.narrowing else {
            return true;
        };
        let haystack = format!(
            "{} {} {} {} {}",
            card.title,
            card.bullets.join(" "),
            card.tags.join(" "),
            card.source_label.as_deref().unwrap_or_default(),
            text
        );
        narrowing.matches(&haystack)
    }

    fn has_keywords(&self) -> bool {
        !self.keywords.is_empty()
    }
}

/// Bumped whenever `SCHEMA` changes in a way that needs migrating.
const SCHEMA_VERSION: i64 = 7;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS articles (
    id             INTEGER PRIMARY KEY,
    canonical_url  TEXT NOT NULL UNIQUE,
    url            TEXT NOT NULL,
    source_label   TEXT,
    title          TEXT NOT NULL DEFAULT '',
    author         TEXT,
    text           TEXT NOT NULL DEFAULT '',
    excerpt        TEXT,
    lang           TEXT,
    published_at   TEXT,
    date_uncertain INTEGER NOT NULL DEFAULT 0,
    status         TEXT NOT NULL,
    status_reason  TEXT,
    discussion_url TEXT,
    score          INTEGER,
    comments       INTEGER,
    simhash        INTEGER,
    cluster_id     INTEGER,
    fetched_at     TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_articles_published ON articles (published_at DESC);
CREATE INDEX IF NOT EXISTS idx_articles_status    ON articles (status);
CREATE INDEX IF NOT EXISTS idx_articles_cluster   ON articles (cluster_id);

CREATE TABLE IF NOT EXISTS summaries (
    id             INTEGER PRIMARY KEY,
    article_id     INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    target_lang    TEXT NOT NULL,
    model_id       TEXT NOT NULL,
    prompt_version TEXT NOT NULL,
    title          TEXT NOT NULL,
    bullets        TEXT NOT NULL,
    tags           TEXT NOT NULL,
    relevance      REAL NOT NULL,
    created_at     TEXT NOT NULL,
    UNIQUE (article_id, target_lang, model_id, prompt_version)
);

CREATE TABLE IF NOT EXISTS audio (
    id         INTEGER PRIMARY KEY,
    summary_id INTEGER NOT NULL REFERENCES summaries(id) ON DELETE CASCADE,
    engine     TEXT NOT NULL,
    voice      TEXT NOT NULL,
    rate       REAL NOT NULL,
    path       TEXT NOT NULL,
    duration   REAL NOT NULL,
    created_at TEXT NOT NULL,
    UNIQUE (summary_id, engine, voice, rate)
);

CREATE TABLE IF NOT EXISTS boilerplate (
    domain        TEXT NOT NULL,
    fragment_hash INTEGER NOT NULL,
    seen_count    INTEGER NOT NULL DEFAULT 1,
    PRIMARY KEY (domain, fragment_hash)
);

-- An article one model would not write about, remembered against that model.
--
-- A service can refuse a particular article and say only "bad request";
-- without this the article is offered again on every run for ever. Keyed by
-- the model, so choosing another puts the article back in the queue.
CREATE TABLE IF NOT EXISTS refused (
    article_id INTEGER NOT NULL REFERENCES articles(id) ON DELETE CASCADE,
    model_id   TEXT NOT NULL,
    code       TEXT NOT NULL,
    detail     TEXT,
    seen_at    TEXT NOT NULL,
    PRIMARY KEY (article_id, model_id)
);

-- Whether each source answered, and what it said if it did not.
--
-- No ETag: every run asks the whole question and takes the whole answer, so a
-- result depends on the window asked for rather than on when the program last
-- ran.
CREATE TABLE IF NOT EXISTS source_state (
    url           TEXT PRIMARY KEY,
    last_seen     TEXT,
    failed_at     TEXT,
    failure       TEXT
);

CREATE VIRTUAL TABLE IF NOT EXISTS articles_fts USING fts5 (
    title, text,
    content='articles', content_rowid='id',
    tokenize='unicode61 remove_diacritics 2'
);

CREATE TRIGGER IF NOT EXISTS articles_ai AFTER INSERT ON articles BEGIN
    INSERT INTO articles_fts (rowid, title, text) VALUES (new.id, new.title, new.text);
END;
CREATE TRIGGER IF NOT EXISTS articles_ad AFTER DELETE ON articles BEGIN
    INSERT INTO articles_fts (articles_fts, rowid, title, text)
    VALUES ('delete', old.id, old.title, old.text);
END;
CREATE TRIGGER IF NOT EXISTS articles_au AFTER UPDATE ON articles BEGIN
    INSERT INTO articles_fts (articles_fts, rowid, title, text)
    VALUES ('delete', old.id, old.title, old.text);
    INSERT INTO articles_fts (rowid, title, text) VALUES (new.id, new.title, new.text);
END;
"#;

impl Store {
    pub fn open(path: &Path) -> AppResult<Store> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> AppResult<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> AppResult<Store> {
        // WAL keeps a long write (a summarize run) from blocking the UI's reads,
        // and survives an abrupt exit — which matters because a forced quit is a
        // supported way to leave this program.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;

        let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version < 2 {
            // `CREATE TABLE IF NOT EXISTS` cannot add a column to a database
            // that already exists, so old installations need this.
            let _ = conn.execute("ALTER TABLE articles ADD COLUMN status_reason TEXT", []);
        }
        if version < SCHEMA_VERSION {
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        // `CREATE TABLE IF NOT EXISTS` above adds `refused` to a database that
        // lacks it, so version 6 needs nothing further.
        if version < 5 {
            // Same reason as version 2: an existing table gains no columns from
            // `CREATE TABLE IF NOT EXISTS`.
            let _ = conn.execute("ALTER TABLE source_state ADD COLUMN failed_at TEXT", []);
            let _ = conn.execute("ALTER TABLE source_state ADD COLUMN failure TEXT", []);
        }
        // `CREATE TABLE IF NOT EXISTS` above adds `audio` to a database that
        // lacks it, so version 3 needs nothing further.
        if version < 7 {
            // The search-engine era left two tables' worth of bookkeeping that
            // nothing reads now: verdicts remembered against a set of keywords,
            // and validators for conditional requests no longer made. Kept
            // around they would be a puzzle for the next person to open the
            // database, so they go rather than sit there.
            let _ = conn.execute("DROP TABLE IF EXISTS off_topic", []);
            let _ = conn.execute("ALTER TABLE source_state DROP COLUMN etag", []);
            let _ = conn.execute("ALTER TABLE source_state DROP COLUMN last_modified", []);
        }
        Ok(Store { conn })
    }

    /// Flush and compact before the process ends.
    ///
    /// Not needed for correctness — WAL replays on the next open — but it keeps
    /// the sidecar files from growing across many runs.
    pub fn checkpoint(&self) -> AppResult<()> {
        self.conn.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")?;
        Ok(())
    }

    /// True if this article is already held, so the pipeline can skip fetching.
    pub fn has_article(&self, canonical_url: &str) -> AppResult<bool> {
        let found: Option<i64> = self
            .conn
            .query_row(
                "SELECT id FROM articles WHERE canonical_url = ?1",
                params![canonical_url],
                |r| r.get(0),
            )
            .optional()?;
        Ok(found.is_some())
    }

    /// Insert or update an article, returning its row id.
    pub fn upsert_article(
        &self,
        a: &Article,
        source_label: &str,
        status_reason: Option<&str>,
    ) -> AppResult<i64> {
        self.conn.execute(
            "INSERT INTO articles (
                 canonical_url, url, source_label, title, author, text, excerpt, lang,
                 published_at, date_uncertain, status, status_reason, discussion_url,
                 score, comments, simhash, fetched_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17)
             ON CONFLICT(canonical_url) DO UPDATE SET
                 title=excluded.title, text=excluded.text, excerpt=excluded.excerpt,
                 status=excluded.status, status_reason=excluded.status_reason,
                 score=excluded.score, comments=excluded.comments,
                 simhash=excluded.simhash, fetched_at=excluded.fetched_at",
            params![
                a.canonical_url,
                a.url,
                source_label,
                a.title,
                a.author,
                a.text,
                a.excerpt,
                a.lang.map(|l| l.code()),
                a.published_at.map(|d| d.to_rfc3339()),
                a.date_uncertain as i64,
                a.status.as_str(),
                status_reason,
                a.discussion_url,
                a.metrics.score,
                a.metrics.comments,
                None::<i64>,
                Utc::now().to_rfc3339(),
            ],
        )?;
        let id: i64 = self.conn.query_row(
            "SELECT id FROM articles WHERE canonical_url = ?1",
            params![a.canonical_url],
            |r| r.get(0),
        )?;
        Ok(id)
    }

    pub fn set_status(
        &self,
        article_id: i64,
        status: Status,
        reason: Option<&str>,
    ) -> AppResult<()> {
        self.conn.execute(
            "UPDATE articles SET status = ?2, status_reason = ?3 WHERE id = ?1",
            params![article_id, status.as_str(), reason],
        )?;
        Ok(())
    }

    /// How many articles each verdict accounts for, with the reasons given.
    ///
    /// Bounded by source and date but not by keywords: a rejected article was
    /// never tested against them.
    pub fn verdict_breakdown(
        &self,
        selection: &Selection,
    ) -> AppResult<Vec<(String, Option<String>, i64)>> {
        let (clause, args) = selection.clause("a");
        let sql = format!(
            "SELECT a.status, a.status_reason, COUNT(*) FROM articles a
             WHERE 1{clause}
             GROUP BY a.status, a.status_reason ORDER BY 3 DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_simhash(&self, article_id: i64, simhash: u64) -> AppResult<()> {
        self.conn.execute(
            "UPDATE articles SET simhash = ?2 WHERE id = ?1",
            params![article_id, simhash as i64],
        )?;
        Ok(())
    }

    /// Every (id, simhash) held, for near-duplicate comparison.
    pub fn simhashes(&self) -> AppResult<Vec<(i64, u64)>> {
        let mut stmt =
            self.conn.prepare("SELECT id, simhash FROM articles WHERE simhash IS NOT NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)? as u64)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn set_cluster(&self, article_id: i64, cluster_id: i64) -> AppResult<()> {
        self.conn.execute(
            "UPDATE articles SET cluster_id = ?2 WHERE id = ?1",
            params![article_id, cluster_id],
        )?;
        Ok(())
    }

    /// Look up a cached summary, so a second run costs nothing.
    pub fn summary(
        &self,
        article_id: i64,
        lang: Lang,
        model_id: &str,
        prompt_version: &str,
    ) -> AppResult<Option<ArticleSummary>> {
        let row = self
            .conn
            .query_row(
                "SELECT title, bullets, tags, relevance FROM summaries
                 WHERE article_id=?1 AND target_lang=?2 AND model_id=?3 AND prompt_version=?4",
                params![article_id, lang.code(), model_id, prompt_version],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, f64>(3)?,
                    ))
                },
            )
            .optional()?;

        Ok(row.map(|(title, bullets, tags, relevance)| ArticleSummary {
            title,
            bullets: serde_json::from_str(&bullets).unwrap_or_default(),
            tags: serde_json::from_str(&tags).unwrap_or_default(),
            relevance: relevance as f32,
        }))
    }

    pub fn save_summary(
        &self,
        article_id: i64,
        lang: Lang,
        model_id: &str,
        prompt_version: &str,
        s: &ArticleSummary,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO summaries
                 (article_id, target_lang, model_id, prompt_version,
                  title, bullets, tags, relevance, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
             ON CONFLICT(article_id, target_lang, model_id, prompt_version) DO UPDATE SET
                 title=excluded.title, bullets=excluded.bullets, tags=excluded.tags,
                 relevance=excluded.relevance, created_at=excluded.created_at",
            params![
                article_id,
                lang.code(),
                model_id,
                prompt_version,
                s.title,
                serde_json::to_string(&s.bullets).unwrap_or_else(|_| "[]".into()),
                serde_json::to_string(&s.tags).unwrap_or_else(|_| "[]".into()),
                s.relevance as f64,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Record that a fragment was seen on a domain, returning how many distinct
    /// articles of that domain have now shown it.
    pub fn note_fragment(&self, domain: &str, fragment_hash: u64) -> AppResult<i64> {
        self.conn.execute(
            "INSERT INTO boilerplate (domain, fragment_hash, seen_count) VALUES (?1, ?2, 1)
             ON CONFLICT(domain, fragment_hash) DO UPDATE SET seen_count = seen_count + 1",
            params![domain, fragment_hash as i64],
        )?;
        let count: i64 = self.conn.query_row(
            "SELECT seen_count FROM boilerplate WHERE domain=?1 AND fragment_hash=?2",
            params![domain, fragment_hash as i64],
            |r| r.get(0),
        )?;
        Ok(count)
    }

    /// Fragments of `domain` seen at least `threshold` times: the site's own
    /// furniture, to be stripped from every article extracted from it.
    pub fn known_boilerplate(&self, domain: &str, threshold: i64) -> AppResult<Vec<u64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT fragment_hash FROM boilerplate WHERE domain=?1 AND seen_count>=?2")?;
        let rows = stmt
            .query_map(params![domain, threshold], |r| Ok(r.get::<_, i64>(0)? as u64))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Remember that this source could not be reached, and why.
    ///
    /// Per source rather than counted per run, so a feed that has been dead for
    /// a month can be told apart from a quiet week.
    pub fn note_source_failure(&self, url: &str, why: &str) -> AppResult<()> {
        // Trimmed: some servers answer a failure with a page of HTML, and none
        // of it belongs in a tooltip.
        let why: String = why.chars().take(200).collect();
        self.conn.execute(
            "INSERT INTO source_state (url, failed_at, failure) VALUES (?1,?2,?3)
             ON CONFLICT(url) DO UPDATE SET failed_at=excluded.failed_at, failure=excluded.failure",
            params![url, Utc::now().to_rfc3339(), why],
        )?;
        Ok(())
    }

    /// This source answered, so whatever went wrong last time is over. The mark
    /// means "did not answer last time", not "has ever failed".
    /// Remember that `model_id` would not write about this article.
    pub fn note_refusal(
        &self,
        article_id: i64,
        model_id: &str,
        code: &str,
        detail: Option<&str>,
    ) -> AppResult<()> {
        let detail: Option<String> = detail.map(|d| d.chars().take(300).collect());
        self.conn.execute(
            "INSERT INTO refused (article_id, model_id, code, detail, seen_at)
             VALUES (?1,?2,?3,?4,?5)
             ON CONFLICT(article_id, model_id) DO UPDATE SET
                 code=excluded.code, detail=excluded.detail, seen_at=excluded.seen_at",
            params![article_id, model_id, code, detail, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Articles some model refused, counted by the reason it gave.
    ///
    /// Grouped by code, not by text: the text carries a request id, so every
    /// refusal would otherwise be its own line.
    pub fn refusal_breakdown(
        &self,
        selection: &Selection,
    ) -> AppResult<Vec<(String, Option<String>, i64)>> {
        let (clause, args) = selection.clause("a");
        let sql = format!(
            "SELECT r.code, MIN(r.detail), COUNT(*) FROM refused r
             JOIN articles a ON a.id = r.article_id
             WHERE 1{clause}
             GROUP BY r.code ORDER BY 3 DESC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt
            .query_map(params_from_iter(args), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn note_source_reached(&self, url: &str) -> AppResult<()> {
        self.conn.execute(
            "UPDATE source_state SET failed_at = NULL, failure = NULL WHERE url = ?1",
            params![url],
        )?;
        Ok(())
    }

    /// The last failure of this source, if its last attempt was one.
    pub fn source_failure(&self, url: &str) -> AppResult<Option<String>> {
        let row = self
            .conn
            .query_row(
                "SELECT failure FROM source_state WHERE url = ?1 AND failed_at IS NOT NULL",
                params![url],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        Ok(row.flatten())
    }

    /// The next articles to summarize, within the current selection:
    /// extracted cleanly, with no summary for this (language, model, prompt).
    ///
    /// The limit is applied after the keyword test rather than in SQL, or a
    /// filter that rejects half the rows would return half a batch.
    pub fn pending_summaries(
        &self,
        lang: Lang,
        model_id: &str,
        prompt_version: &str,
        limit: usize,
        selection: &Selection,
    ) -> AppResult<Vec<StoredArticle>> {
        let (clause, filter_args) = selection.clause("a");
        let sql = format!(
            "SELECT a.id, a.canonical_url, a.title, a.text, a.lang, a.published_at, a.source_label
             FROM articles a
             LEFT JOIN summaries s
               ON s.article_id = a.id AND s.target_lang = ?
              AND s.model_id = ? AND s.prompt_version = ?
             WHERE a.status = 'ok' AND s.id IS NULL{clause}
               AND NOT EXISTS (
                   SELECT 1 FROM refused r
                    WHERE r.article_id = a.id AND r.model_id = ?
               )
             ORDER BY a.published_at DESC NULLS LAST, a.id DESC"
        );

        let mut args = vec![
            Value::from(lang.code().to_string()),
            Value::from(model_id.to_string()),
            Value::from(prompt_version.to_string()),
        ];
        args.extend(filter_args);
        // The exclusion's own parameter comes after the selection's, because
        // that is the order they appear in the statement.
        args.push(Value::from(model_id.to_string()));

        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        let mut found = Vec::new();
        while let Some(r) = rows.next()? {
            let text: String = r.get(3)?;
            if !selection.allows(&text) {
                continue;
            }
            found.push(StoredArticle {
                id: r.get(0)?,
                canonical_url: r.get(1)?,
                title: r.get(2)?,
                text,
                lang: r.get::<_, Option<String>>(4)?.and_then(|s| s.parse().ok()),
                published_at: parse_time(r.get::<_, Option<String>>(5)?),
                source_label: r.get(6)?,
            });
            if found.len() >= limit {
                break;
            }
        }
        Ok(found)
    }

    /// Summaries ready to display, newest first, one row per duplicate cluster.
    ///
    /// `skip` is how many matching cards the caller already has. Counted here
    /// rather than with SQL `OFFSET`: the stem tests run in Rust, so the
    /// database cannot tell which rows match.
    pub fn recent_summaries(
        &self,
        lang: Lang,
        prompt_version: &str,
        limit: usize,
        skip: usize,
        selection: &Selection,
    ) -> AppResult<Vec<Card>> {
        let (outer, outer_args) = selection.clause("a");
        // The cluster representative has to be chosen from within the selection
        // too. Picking it from everything would drop a whole cluster whenever
        // its representative came from a source the reader has turned off — the
        // story would vanish although a selected source also carried it.
        let (inner, inner_args) = selection.clause("a2");
        let sql = format!(
            "SELECT s.id, s.title, s.bullets, s.tags, s.relevance,
                    a.canonical_url, a.published_at, a.source_label, a.text
             FROM summaries s
             JOIN articles a ON a.id = s.article_id
             WHERE s.target_lang = ? AND s.prompt_version = ?{outer}
               AND s.id = (SELECT MIN(s2.id) FROM summaries s2
                           JOIN articles a2 ON a2.id = s2.article_id
                           WHERE a2.cluster_id = a.cluster_id
                             AND s2.target_lang = s.target_lang
                             AND s2.prompt_version = s.prompt_version{inner})
             ORDER BY a.published_at DESC NULLS LAST, s.id DESC"
        );

        let mut args =
            vec![Value::from(lang.code().to_string()), Value::from(prompt_version.to_string())];
        args.extend(outer_args);
        args.extend(inner_args);

        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        let mut cards = Vec::new();
        let mut passed_over = 0usize;
        while let Some(r) = rows.next()? {
            let text: String = r.get(8)?;
            if !selection.allows(&text) {
                continue;
            }
            let card = Card {
                id: r.get(0)?,
                title: r.get(1)?,
                bullets: serde_json::from_str(&r.get::<_, String>(2)?).unwrap_or_default(),
                tags: serde_json::from_str(&r.get::<_, String>(3)?).unwrap_or_default(),
                relevance: r.get::<_, f64>(4)? as f32,
                url: r.get(5)?,
                published_at: parse_time(r.get::<_, Option<String>>(6)?),
                source_label: r.get(7)?,
            };
            if !selection.narrows_to(&card, &text) {
                continue;
            }
            if passed_over < skip {
                passed_over += 1;
                continue;
            }
            cards.push(card);
            if cards.len() >= limit {
                break;
            }
        }
        Ok(cards)
    }

    /// Everything held, in the form the quality gates need. Used by
    /// `recheck`, which re-judges stored text under the current rules.
    pub fn all_for_recheck(&self) -> AppResult<Vec<StoredArticle>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, canonical_url, title, text, lang, published_at, source_label
             FROM articles ORDER BY id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(StoredArticle {
                    id: r.get(0)?,
                    canonical_url: r.get(1)?,
                    title: r.get(2)?,
                    text: r.get(3)?,
                    lang: r.get::<_, Option<String>>(4)?.and_then(|s| s.parse().ok()),
                    published_at: parse_time(r.get::<_, Option<String>>(5)?),
                    source_label: r.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn status_of(&self, article_id: i64) -> AppResult<Option<Status>> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT status FROM articles WHERE id = ?1", params![article_id], |r| {
                r.get(0)
            })
            .optional()?;
        Ok(raw.and_then(|s| Status::parse(&s)))
    }

    /// Drop summaries written for an article that is no longer considered one.
    pub fn delete_summaries(&self, article_id: i64) -> AppResult<usize> {
        Ok(self.conn.execute("DELETE FROM summaries WHERE article_id = ?1", params![article_id])?)
    }

    /// The audio already made for this summary with these settings, if the
    /// file is still on disk.
    ///
    /// The file is checked, not just the row: the cache directory may have been
    /// cleared, leaving a row that points at nothing.
    pub fn cached_audio(
        &self,
        summary_id: i64,
        engine: &str,
        voice: &str,
        rate: f32,
    ) -> AppResult<Option<(PathBuf, f32)>> {
        let row: Option<(String, f64)> = self
            .conn
            .query_row(
                "SELECT path, duration FROM audio
                 WHERE summary_id=?1 AND engine=?2 AND voice=?3 AND rate=?4",
                params![summary_id, engine, voice, rate as f64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        Ok(row.and_then(|(path, duration)| {
            let path = PathBuf::from(path);
            path.is_file().then_some((path, duration as f32))
        }))
    }

    pub fn save_audio(
        &self,
        summary_id: i64,
        engine: &str,
        voice: &str,
        rate: f32,
        path: &Path,
        duration: f32,
    ) -> AppResult<()> {
        self.conn.execute(
            "INSERT INTO audio (summary_id, engine, voice, rate, path, duration, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(summary_id, engine, voice, rate) DO UPDATE SET
                 path=excluded.path, duration=excluded.duration, created_at=excluded.created_at",
            params![
                summary_id,
                engine,
                voice,
                rate as f64,
                path.display().to_string(),
                duration as f64,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    /// Total size of the cached audio, and how many files it is.
    pub fn audio_usage(&self) -> AppResult<(u64, i64)> {
        let mut stmt = self.conn.prepare("SELECT path FROM audio")?;
        let paths: Vec<String> =
            stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        let mut bytes = 0u64;
        let mut files = 0i64;
        for p in paths {
            if let Ok(meta) = std::fs::metadata(&p) {
                bytes += meta.len();
                files += 1;
            }
        }
        Ok((bytes, files))
    }

    /// Delete everything older than `before`, with its summaries and recordings.
    ///
    /// By publication date where there is one, by the fetch date where there is
    /// not, so an article with an unreadable date is not immortal.
    pub fn prune(&self, before: DateTime<Utc>) -> AppResult<PruneReport> {
        let cutoff = before.to_rfc3339();
        const OLDER: &str = "COALESCE(a.published_at, a.fetched_at) < ?1";

        // The files first, and by their own path: the rows go with the article
        // through two cascades, and a deleted row can no longer say what it
        // left behind on disk.
        let mut stmt = self.conn.prepare(&format!(
            "SELECT au.path FROM audio au
             JOIN summaries s ON s.id = au.summary_id
             JOIN articles a ON a.id = s.article_id
             WHERE {OLDER}"
        ))?;
        let paths: Vec<String> =
            stmt.query_map(params![cutoff], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        for path in &paths {
            let _ = std::fs::remove_file(path);
        }

        let summaries: i64 = self.conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM summaries s JOIN articles a ON a.id = s.article_id
                 WHERE {OLDER}"
            ),
            params![cutoff],
            |r| r.get(0),
        )?;

        // One statement, two cascades: summaries follow the article, recordings
        // follow the summary. That is what the foreign keys are declared for.
        let articles = self
            .conn
            .execute(&format!("DELETE FROM articles AS a WHERE {OLDER}"), params![cutoff])?
            as i64;

        Ok(PruneReport { articles, summaries, recordings: paths.len() as i64 })
    }

    /// Throw the whole cache away: articles, summaries, recordings, and
    /// everything learned about the sites. Unlike `prune`, regardless of age.
    pub fn clear_all(&self) -> AppResult<PruneReport> {
        let mut stmt = self.conn.prepare("SELECT path FROM audio")?;
        let paths: Vec<String> =
            stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        for path in &paths {
            let _ = std::fs::remove_file(path);
        }

        let summaries: i64 =
            self.conn.query_row("SELECT COUNT(*) FROM summaries", [], |r| r.get(0))?;
        let articles = self.conn.execute("DELETE FROM articles", [])? as i64;

        self.conn.execute_batch(
            "DELETE FROM boilerplate;
             DELETE FROM source_state;",
        )?;

        Ok(PruneReport { articles, summaries, recordings: paths.len() as i64 })
    }

    /// Give the freed pages back to the filesystem.
    ///
    /// Separate from `prune` because it rewrites the whole file: worth doing
    /// after a real deletion, wasteful after one that removed three rows.
    pub fn compact(&self) -> AppResult<()> {
        self.conn.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Forget every recording, deleting the files with the rows.
    pub fn clear_audio(&self) -> AppResult<i64> {
        let mut stmt = self.conn.prepare("SELECT path FROM audio")?;
        let paths: Vec<String> =
            stmt.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        for p in &paths {
            let _ = std::fs::remove_file(p);
        }
        Ok(self.conn.execute("DELETE FROM audio", [])? as i64)
    }

    /// One summary as a passage to read: the headline, then the points.
    pub fn summary_text(&self, summary_id: i64) -> AppResult<Option<String>> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT title, bullets FROM summaries WHERE id = ?1",
                params![summary_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        Ok(row.map(|(title, bullets)| {
            let points: Vec<String> = serde_json::from_str(&bullets).unwrap_or_default();
            let mut text = title.trim().trim_end_matches('.').to_string();
            text.push_str(". ");
            text.push_str(&points.join(" "));
            text
        }))
    }

    /// How many articles the current selection holds.
    ///
    /// With keywords set the rows are read rather than counted, the stem test
    /// living in Rust; the selection bounds that scan by source and date.
    pub fn count_articles(&self, selection: &Selection) -> AppResult<i64> {
        let (clause, args) = selection.clause("a");
        if !selection.has_keywords() {
            let sql = format!("SELECT COUNT(*) FROM articles a WHERE 1{clause}");
            return Ok(self.conn.query_row(&sql, params_from_iter(args), |r| r.get(0))?);
        }
        let sql = format!("SELECT a.text FROM articles a WHERE 1{clause}");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        let mut count = 0;
        while let Some(r) = rows.next()? {
            if selection.allows(&r.get::<_, String>(0)?) {
                count += 1;
            }
        }
        Ok(count)
    }

    /// Articles written up in the language being read.
    ///
    /// Articles, not summary rows: one article carries a summary per language,
    /// model and prompt version, so counting rows can exceed the article count.
    pub fn count_summaries(
        &self,
        lang: Lang,
        prompt_version: &str,
        selection: &Selection,
    ) -> AppResult<i64> {
        self.count_with(WRITTEN_UP, lang, prompt_version, selection)
    }

    /// Articles queued: readable, wanted, and not written up yet.
    ///
    /// `model_id` is the model that would do the work; articles it has already
    /// refused are left out, since it will never be offered them again. `None`
    /// counts them, for when no model is known.
    pub fn count_pending(
        &self,
        lang: Lang,
        prompt_version: &str,
        model_id: Option<&str>,
        selection: &Selection,
    ) -> AppResult<i64> {
        let all = self.count_with(WAITING, lang, prompt_version, selection)?;
        let Some(model_id) = model_id else {
            return Ok(all);
        };
        Ok(all - self.count_with(&refused_by(model_id), lang, prompt_version, selection)?)
    }

    /// The two counts above differ by one word of SQL, and both have to walk
    /// the rows themselves when keywords are in play — stems are matched here,
    /// not by the database.
    fn count_with(
        &self,
        cond: &str,
        lang: Lang,
        prompt_version: &str,
        selection: &Selection,
    ) -> AppResult<i64> {
        let (clause, filter_args) = selection.clause("a");
        let mut args: Vec<Value> =
            vec![Value::from(lang.code().to_string()), Value::from(prompt_version.to_string())];
        args.extend(filter_args);

        if !selection.has_keywords() {
            let sql = format!("SELECT COUNT(*) FROM articles a WHERE {cond}{clause}");
            return Ok(self.conn.query_row(&sql, params_from_iter(args), |r| r.get(0))?);
        }
        let sql = format!("SELECT a.text FROM articles a WHERE {cond}{clause}");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query(params_from_iter(args))?;
        let mut count = 0;
        while let Some(r) = rows.next()? {
            if selection.allows(&r.get::<_, String>(0)?) {
                count += 1;
            }
        }
        Ok(count)
    }
}

/// Has a summary in the language being read.
const WRITTEN_UP: &str = "EXISTS (SELECT 1 FROM summaries s
                                   WHERE s.article_id = a.id
                                     AND s.target_lang = ?1 AND s.prompt_version = ?2)";
/// Of those, the ones this model has already turned down. The parameter is
/// spliced in rather than bound, because the two counts share one code path and
/// its placeholders are already spoken for.
fn refused_by(model_id: &str) -> String {
    format!(
        "{WAITING} AND EXISTS (SELECT 1 FROM refused r
                                WHERE r.article_id = a.id AND r.model_id = '{}')",
        model_id.replace('\'', "''")
    )
}

/// Readable and not written up: the queue.
const WAITING: &str = "a.status = 'ok' AND NOT EXISTS (SELECT 1 FROM summaries s
                                   WHERE s.article_id = a.id
                                     AND s.target_lang = ?1 AND s.prompt_version = ?2)";

#[derive(Debug, Default, Clone, Copy)]
pub struct PruneReport {
    pub articles: i64,
    pub summaries: i64,
    pub recordings: i64,
}

/// One rendered card: a summary plus the article facts around it.
#[derive(Debug, Clone)]
pub struct Card {
    /// The summary this came from — what a recording is filed against.
    pub id: i64,
    pub title: String,
    pub bullets: Vec<String>,
    pub tags: Vec<String>,
    pub relevance: f32,
    pub url: String,
    pub published_at: Option<DateTime<Utc>>,
    pub source_label: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredArticle {
    pub id: i64,
    pub canonical_url: String,
    pub title: String,
    pub text: String,
    pub lang: Option<Lang>,
    pub published_at: Option<DateTime<Utc>>,
    pub source_label: Option<String>,
}

fn parse_time(s: Option<String>) -> Option<DateTime<Utc>> {
    s.and_then(|s| DateTime::parse_from_rfc3339(&s).ok()).map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::news::types::{Metrics, Status};

    fn sample(url: &str) -> Article {
        Article {
            url: url.into(),
            canonical_url: url.into(),
            title: "Заголовок".into(),
            author: None,
            text: "Текст статьи".into(),
            published_at: None,
            date_uncertain: true,
            lang: Some(Lang::Ru),
            excerpt: None,
            discussion_url: None,
            metrics: Metrics::default(),
            status: Status::Ok,
        }
    }

    #[test]
    fn an_article_one_model_refused_is_still_offered_to_another() {
        let s = Store::open_in_memory().unwrap();
        let id = s.upsert_article(&sample("https://example.com/refused"), "test", None).unwrap();
        let pending = |model: &str| {
            s.pending_summaries(Lang::Ru, model, "v1", 10, &Selection::everything()).unwrap().len()
        };

        assert_eq!(pending("api:strict"), 1);
        assert_eq!(pending("api:easy"), 1);

        s.note_refusal(id, "api:strict", "api_refused", Some("Provider returned error")).unwrap();

        assert_eq!(pending("api:strict"), 0, "тот же сервис второй раз не спрашиваем");
        assert_eq!(pending("api:easy"), 1, "у другой модели своя очередь");

        // And it is visible rather than silently missing.
        let breakdown = s.refusal_breakdown(&Selection::everything()).unwrap();
        assert_eq!(breakdown.len(), 1);
        assert_eq!(breakdown[0].0, "api_refused");
        assert_eq!(breakdown[0].2, 1);
    }

    #[test]
    fn forgetting_an_article_forgets_that_it_was_refused() {
        // The row hangs off the article, so pruning takes it along rather than
        // leaving a verdict about something that is gone.
        let s = Store::open_in_memory().unwrap();
        let id = s.upsert_article(&sample("https://example.com/old"), "test", None).unwrap();
        s.note_refusal(id, "api:strict", "api_refused", None).unwrap();
        assert_eq!(s.refusal_breakdown(&Selection::everything()).unwrap().len(), 1);

        s.clear_all().unwrap();
        assert!(s.refusal_breakdown(&Selection::everything()).unwrap().is_empty());
    }

    #[test]
    fn a_source_that_answers_stops_being_marked() {
        let s = Store::open_in_memory().unwrap();
        let feed = "https://example.com/feed.xml";
        assert_eq!(s.source_failure(feed).unwrap(), None, "новый источник ни в чём не виноват");

        s.note_source_failure(feed, "Network error: returned HTTP 404 Not Found").unwrap();
        assert!(s.source_failure(feed).unwrap().unwrap().contains("404"));

        // The mark says "did not answer last time", so answering takes it off.
        s.note_source_reached(feed).unwrap();
        assert_eq!(s.source_failure(feed).unwrap(), None);
    }

    #[test]
    fn a_server_that_answers_a_failure_with_a_page_does_not_fill_the_tooltip() {
        let s = Store::open_in_memory().unwrap();
        let feed = "https://example.com/wordy.xml";
        s.note_source_failure(feed, &"э".repeat(5_000)).unwrap();
        assert_eq!(s.source_failure(feed).unwrap().unwrap().chars().count(), 200);
    }

    #[test]
    fn upsert_is_idempotent() {
        let s = Store::open_in_memory().unwrap();
        let a = sample("https://example.com/1");
        let first = s.upsert_article(&a, "test", None).unwrap();
        let second = s.upsert_article(&a, "test", None).unwrap();
        assert_eq!(first, second);
        assert_eq!(s.count_articles(&Selection::everything()).unwrap(), 1);
    }

    #[test]
    fn summary_is_cached_per_model_and_prompt() {
        let s = Store::open_in_memory().unwrap();
        let id = s.upsert_article(&sample("https://example.com/2"), "test", None).unwrap();
        let sum = ArticleSummary {
            title: "T".into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.7,
        };
        s.save_summary(id, Lang::Ru, "m1", "v1", &sum).unwrap();

        assert!(s.summary(id, Lang::Ru, "m1", "v1").unwrap().is_some());
        // A different model or prompt is a different result, not a cache hit.
        assert!(s.summary(id, Lang::Ru, "m2", "v1").unwrap().is_none());
        assert!(s.summary(id, Lang::Ru, "m1", "v2").unwrap().is_none());
        assert!(s.summary(id, Lang::En, "m1", "v1").unwrap().is_none());
    }

    #[test]
    fn pending_excludes_already_summarized() {
        let s = Store::open_in_memory().unwrap();
        let id = s.upsert_article(&sample("https://example.com/3"), "test", None).unwrap();
        assert_eq!(
            s.pending_summaries(Lang::Ru, "m1", "v1", 10, &Selection::everything()).unwrap().len(),
            1
        );

        let sum = ArticleSummary {
            title: "T".into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };
        s.save_summary(id, Lang::Ru, "m1", "v1", &sum).unwrap();
        assert_eq!(
            s.pending_summaries(Lang::Ru, "m1", "v1", 10, &Selection::everything()).unwrap().len(),
            0
        );
        // …but it is still pending for another output language.
        assert_eq!(
            s.pending_summaries(Lang::En, "m1", "v1", 10, &Selection::everything()).unwrap().len(),
            1
        );
    }

    #[test]
    fn boilerplate_accumulates_per_domain() {
        let s = Store::open_in_memory().unwrap();
        assert_eq!(s.note_fragment("example.com", 42).unwrap(), 1);
        assert_eq!(s.note_fragment("example.com", 42).unwrap(), 2);
        assert_eq!(s.note_fragment("other.com", 42).unwrap(), 1);
        assert_eq!(s.known_boilerplate("example.com", 2).unwrap(), vec![42]);
        assert!(s.known_boilerplate("other.com", 2).unwrap().is_empty());
    }

    #[test]
    fn only_one_card_per_duplicate_cluster() {
        let s = Store::open_in_memory().unwrap();
        let sum = |t: &str| ArticleSummary {
            title: t.into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };

        let a = s.upsert_article(&sample("https://one.example/x"), "A", None).unwrap();
        let b = s.upsert_article(&sample("https://two.example/x"), "B", None).unwrap();
        // Both articles are the same story: B points at A's cluster.
        s.set_cluster(a, a).unwrap();
        s.set_cluster(b, a).unwrap();
        s.save_summary(a, Lang::Ru, "m", "v", &sum("Первая")).unwrap();
        s.save_summary(b, Lang::Ru, "m", "v", &sum("Вторая")).unwrap();

        let cards = s.recent_summaries(Lang::Ru, "v", 10, 0, &Selection::everything()).unwrap();
        assert_eq!(cards.len(), 1, "duplicates must collapse into one card");
        assert_eq!(cards[0].title, "Первая");
    }

    /// A selection built by hand, the way `from_config` would.
    fn selection(sources: &[&str], keywords: &[&str], since: Option<DateTime<Utc>>) -> Selection {
        Selection {
            sources: Some(sources.iter().map(|s| s.to_string()).collect()),
            since,
            until: None,
            keywords: KeywordFilter::new(
                &keywords.iter().map(|k| k.to_string()).collect::<Vec<_>>(),
                Lang::Ru,
            ),
            narrowing: None,
        }
    }

    #[test]
    fn a_source_switched_off_leaves_the_feed_and_the_queue() {
        let s = Store::open_in_memory().unwrap();
        let sum = ArticleSummary {
            title: "Т".into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };
        let kept = s.upsert_article(&sample("https://kept.example/1"), "Оставлен", None).unwrap();
        let gone = s.upsert_article(&sample("https://gone.example/1"), "Выключен", None).unwrap();
        s.set_cluster(kept, kept).unwrap();
        s.set_cluster(gone, gone).unwrap();
        s.save_summary(kept, Lang::Ru, "m", "v", &sum).unwrap();
        s.save_summary(gone, Lang::Ru, "m", "v", &sum).unwrap();

        let only_one = selection(&["Оставлен"], &[], None);
        let cards = s.recent_summaries(Lang::Ru, "v", 10, 0, &only_one).unwrap();
        assert_eq!(cards.len(), 1, "выключенный источник не должен показываться");
        assert_eq!(cards[0].source_label.as_deref(), Some("Оставлен"));

        // And the same for work not yet done: an article from a source the
        // reader turned off must not cost a minute of inference.
        assert_eq!(s.count_articles(&only_one).unwrap(), 1);
        assert_eq!(
            s.pending_summaries(Lang::En, "m", "v", 10, &only_one).unwrap().len(),
            1,
            "к переводу на другой язык должна быть предложена только выбранная статья"
        );

        // Nothing was deleted: switching it back on brings it straight back.
        let both = selection(&["Оставлен", "Выключен"], &[], None);
        assert_eq!(s.recent_summaries(Lang::Ru, "v", 10, 0, &both).unwrap().len(), 2);
    }

    #[test]
    fn every_article_is_either_written_up_waiting_or_turned_away() {
        let s = Store::open_in_memory().unwrap();
        let sum = ArticleSummary {
            title: "Т".into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };

        // Three articles: one written up, one waiting its turn, one the gates
        // turned away.
        let done = s.upsert_article(&sample("https://example.com/1"), "И", None).unwrap();
        let waits = s.upsert_article(&sample("https://example.com/2"), "И", None).unwrap();
        let mut bad = sample("https://example.com/3");
        bad.status = Status::LowQuality;
        s.upsert_article(&bad, "И", None).unwrap();
        s.save_summary(done, Lang::Ru, "m", "v", &sum).unwrap();

        let all = Selection::everything();
        assert_eq!(s.count_articles(&all).unwrap(), 3);
        assert_eq!(s.count_summaries(Lang::Ru, "v", &all).unwrap(), 1);
        assert_eq!(s.count_pending(Lang::Ru, "v", None, &all).unwrap(), 1, "{waits} ждёт");

        // A second model on the same article is the same article, not a second
        // summary: counting rows put more summaries on the screen than there
        // were articles as soon as anyone switched model.
        s.save_summary(done, Lang::Ru, "m2", "v", &sum).unwrap();
        assert_eq!(s.count_summaries(Lang::Ru, "v", &all).unwrap(), 1);

        // Reading in another language, nothing is written up and the readable
        // ones are all waiting — which is exactly what the feed will show.
        assert_eq!(s.count_summaries(Lang::En, "v", &all).unwrap(), 0);
        assert_eq!(s.count_pending(Lang::En, "v", None, &all).unwrap(), 2);

        // A model that has already refused an article will never be offered it
        // again, so it must not be promised either: counted for nobody in
        // particular it is still one, counted for that model it is none.
        s.note_refusal(waits, "m", "api_refused", None).unwrap();
        assert_eq!(s.count_pending(Lang::Ru, "v", None, &all).unwrap(), 1);
        assert_eq!(s.count_pending(Lang::Ru, "v", Some("m"), &all).unwrap(), 0);
        // Another model has refused nothing and still has the work to do.
        assert_eq!(s.count_pending(Lang::Ru, "v", Some("m2"), &all).unwrap(), 1);
    }

    #[test]
    fn the_feed_hands_out_pages_that_do_not_overlap() {
        let s = Store::open_in_memory().unwrap();
        let sum = |n: usize| ArticleSummary {
            title: format!("Статья {n}"),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };
        for n in 0..5 {
            let mut a = sample(&format!("https://example.com/{n}"));
            // Newest last, so the feed's own order is the reverse of this.
            a.published_at = Some(Utc::now() - chrono::Duration::hours(5 - n as i64));
            let id = s.upsert_article(&a, "И", None).unwrap();
            s.set_cluster(id, id).unwrap();
            s.save_summary(id, Lang::Ru, "m", "v", &sum(n)).unwrap();
        }

        let all = Selection::everything();
        let page = |skip| {
            s.recent_summaries(Lang::Ru, "v", 2, skip, &all)
                .unwrap()
                .into_iter()
                .map(|c| c.title)
                .collect::<Vec<_>>()
        };

        // Three pages of two, and the last one short: every article once, in
        // order, with nothing repeated between pages.
        let (first, second, third) = (page(0), page(2), page(4));
        assert_eq!(first.len(), 2);
        assert_eq!(second.len(), 2);
        assert_eq!(third.len(), 1, "последняя страница короче — значит конец");

        let mut seen: Vec<String> = Vec::new();
        seen.extend(first);
        seen.extend(second);
        seen.extend(third);
        assert_eq!(seen, vec!["Статья 4", "Статья 3", "Статья 2", "Статья 1", "Статья 0"]);

        // Past the end is empty rather than the start again.
        assert!(page(5).is_empty());
    }

    #[test]
    fn searching_the_feed_looks_at_the_summary_as_well_as_the_article() {
        let s = Store::open_in_memory().unwrap();
        let card = |title: &str| ArticleSummary {
            title: title.into(),
            bullets: vec!["подробность".into()],
            tags: vec![],
            relevance: 0.5,
        };

        let mut about = sample("https://example.com/1");
        about.text = "в тексте речь о санкциях против банка".into();
        let mut other = sample("https://example.com/2");
        other.text = "совсем про другое".into();

        let a = s.upsert_article(&about, "Источник", None).unwrap();
        let b = s.upsert_article(&other, "Источник", None).unwrap();
        s.set_cluster(a, a).unwrap();
        s.set_cluster(b, b).unwrap();
        // The summary is written in the reader's language; the article need not
        // be. Only the second one says "выборы", and it says it on the card.
        s.save_summary(a, Lang::Ru, "m", "v", &card("Банки")).unwrap();
        s.save_summary(b, Lang::Ru, "m", "v", &card("Выборы в Европе")).unwrap();

        let all = Selection::everything();
        assert_eq!(s.recent_summaries(Lang::Ru, "v", 10, 0, &all).unwrap().len(), 2);

        // A word from the article's own text finds it.
        let banks = Selection::everything().searching("санкции", Lang::Ru);
        let found = s.recent_summaries(Lang::Ru, "v", 10, 0, &banks).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].title, "Банки");

        // A word the reader can see on the card finds it too, even though the
        // article behind it never uses that word.
        let elections = Selection::everything().searching("выборами", Lang::Ru);
        let found = s.recent_summaries(Lang::Ru, "v", 10, 0, &elections).unwrap();
        assert_eq!(found.len(), 1, "искать надо по стемам, как и везде здесь");
        assert_eq!(found[0].title, "Выборы в Европе");

        // Nothing typed is not a search for nothing.
        assert_eq!(
            s.recent_summaries(Lang::Ru, "v", 10, 0, &all.searching("  ", Lang::Ru)).unwrap().len(),
            2
        );
    }

    #[test]
    fn the_window_and_the_keywords_bound_the_feed() {
        let s = Store::open_in_memory().unwrap();
        let sum = ArticleSummary {
            title: "Т".into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };

        let mut old_one = sample("https://example.com/old");
        old_one.published_at = Some(Utc::now() - chrono::Duration::days(30));
        old_one.text = "статья про санкции".into();
        let mut new_one = sample("https://example.com/new");
        new_one.published_at = Some(Utc::now());
        new_one.text = "статья про санкции".into();
        let mut about_football = sample("https://example.com/sport");
        about_football.published_at = Some(Utc::now());
        about_football.text = "статья про футбол".into();

        for a in [&old_one, &new_one, &about_football] {
            let id = s.upsert_article(a, "Источник", None).unwrap();
            s.set_cluster(id, id).unwrap();
            s.save_summary(id, Lang::Ru, "m", "v", &sum).unwrap();
        }

        let recent = selection(&["Источник"], &[], Some(Utc::now() - chrono::Duration::days(3)));
        assert_eq!(s.recent_summaries(Lang::Ru, "v", 10, 0, &recent).unwrap().len(), 2);

        // Stems, not substrings: "санкции" has to find "санкциями" too, which
        // is why this test lives here and not in SQL.
        let searched =
            selection(&["Источник"], &["санкциями"], Some(Utc::now() - chrono::Duration::days(3)));
        let cards = s.recent_summaries(Lang::Ru, "v", 10, 0, &searched).unwrap();
        assert_eq!(cards.len(), 1, "поиск должен отсечь всё, кроме одной статьи");
        assert_eq!(s.count_articles(&searched).unwrap(), 1);
    }

    #[test]
    fn old_articles_are_forgotten_with_everything_that_hung_off_them() {
        let s = Store::open_in_memory().unwrap();
        let sum = ArticleSummary {
            title: "Т".into(),
            bullets: vec!["a".into()],
            tags: vec![],
            relevance: 0.5,
        };

        let mut old = sample("https://example.com/old");
        old.published_at = Some(Utc::now() - chrono::Duration::days(200));
        let mut recent = sample("https://example.com/recent");
        recent.published_at = Some(Utc::now());

        let old_id = s.upsert_article(&old, "И", None).unwrap();
        let recent_id = s.upsert_article(&recent, "И", None).unwrap();
        s.save_summary(old_id, Lang::Ru, "m", "v", &sum).unwrap();
        s.save_summary(recent_id, Lang::Ru, "m", "v", &sum).unwrap();

        let report = s.prune(Utc::now() - chrono::Duration::days(90)).unwrap();
        assert_eq!(report.articles, 1);
        assert_eq!(report.summaries, 1, "выжимка уходит вместе со статьёй");
        assert_eq!(s.count_articles(&Selection::everything()).unwrap(), 1);
        assert_eq!(s.count_summaries(Lang::Ru, "v", &Selection::everything()).unwrap(), 1);
        assert!(s.summary(old_id, Lang::Ru, "m", "v").unwrap().is_none());
        assert!(s.summary(recent_id, Lang::Ru, "m", "v").unwrap().is_some());
    }

    #[test]
    fn an_article_with_no_date_is_judged_by_when_we_fetched_it() {
        let s = Store::open_in_memory().unwrap();
        // `sample` has no published_at, so fetched_at — set to now on insert —
        // is what decides. Nothing collected today may be deleted today.
        let id = s.upsert_article(&sample("https://example.com/undated"), "И", None).unwrap();
        assert_eq!(s.prune(Utc::now() - chrono::Duration::days(1)).unwrap().articles, 0);
        assert_eq!(s.prune(Utc::now() + chrono::Duration::days(1)).unwrap().articles, 1);
        assert!(s.status_of(id).unwrap().is_none(), "строки больше нет");
    }

    #[test]
    fn clearing_the_cache_leaves_nothing_behind() {
        let s = Store::open_in_memory().unwrap();
        let id = s.upsert_article(&sample("https://example.com/x"), "И", None).unwrap();
        s.save_summary(
            id,
            Lang::Ru,
            "m",
            "v",
            &ArticleSummary {
                title: "Т".into(),
                bullets: vec!["a".into()],
                tags: vec![],
                relevance: 0.5,
            },
        )
        .unwrap();
        s.note_fragment("example.com", 7).unwrap();

        let report = s.clear_all().unwrap();
        assert_eq!((report.articles, report.summaries), (1, 1));

        let all = Selection::everything();
        assert_eq!(s.count_articles(&all).unwrap(), 0);
        assert_eq!(s.count_summaries(Lang::Ru, "v", &all).unwrap(), 0);
        assert!(s.known_boilerplate("example.com", 1).unwrap().is_empty());
    }

    #[test]
    fn low_quality_articles_are_not_offered_for_summarizing() {
        let s = Store::open_in_memory().unwrap();
        let mut a = sample("https://example.com/4");
        a.status = Status::LowQuality;
        s.upsert_article(&a, "test", None).unwrap();
        assert!(
            s.pending_summaries(Lang::Ru, "m1", "v1", 10, &Selection::everything())
                .unwrap()
                .is_empty()
        );
    }
}
