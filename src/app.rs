// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Tauri commands and the window's event plumbing.
//!
//! Long jobs run on their own thread with their own SQLite connection.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use tauri_plugin_dialog::DialogExt;

use crate::config::{Config, FieldMap, Kind, Selectors, SourceSpec};
use crate::error::{AppError, AppResult};
use crate::llm::{Engine, Lang, Mode};
use crate::models::{Hardware, downloader, registry};
use crate::news::Fetcher;
use crate::paths;
use crate::pipeline;
use crate::shutdown::{Cancel, Shutdown};
use crate::store::{Selection, Store};

pub struct AppState {
    /// The loaded model, kept across jobs.
    engine: Arc<Engine>,
    /// Reads summaries aloud. Owned here because playback outlives the command
    /// that starts it — the window asks for the state afterwards.
    player: Arc<crate::tts::Player>,
    /// Cancel token of the job in flight, if any.
    current: Mutex<Option<Cancel>>,
    busy: AtomicBool,
}

impl AppState {
    fn new() -> AppState {
        AppState {
            engine: Arc::new(Engine::new()),
            player: crate::tts::Player::new(),
            current: Mutex::new(None),
            busy: AtomicBool::new(false),
        }
    }

    /// Claim the right to run a job, or report that one is already running.
    /// One job at a time is a real constraint, not a simplification: the model
    /// is single and the sources deserve one polite client, not two.
    fn claim(&self) -> Option<Cancel> {
        if self.busy.swap(true, Ordering::SeqCst) {
            return None;
        }
        let cancel = Cancel::new();
        *self.current.lock() = Some(cancel.clone());
        Some(cancel)
    }

    fn release(&self) {
        *self.current.lock() = None;
        self.busy.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// what the window receives
// ---------------------------------------------------------------------------

/// The model to fetch when there is none, and what it costs.
///
/// Values rather than a formatted sentence, so the window can write the number
/// in its own locale. The id is what its download button needs.
#[derive(Serialize)]
pub struct ModelHint {
    id: String,
    name: String,
    gigabytes: f64,
}

#[derive(Serialize)]
pub struct Overview {
    hardware: String,
    model_name: Option<String>,
    model_hint: Option<ModelHint>,
    model_loaded: bool,
    ui_lang: String,
    /// Who writes the summaries, and which saved service when it is not this machine.
    provider: String,
    api_url: Option<String>,
    api_model: Option<String>,
    output_lang: String,
    keywords: Vec<String>,
    speech_engine: String,
    speech_voice: Option<String>,
    speech_rate: f32,
    audio_megabytes: f64,
    audio_files: i64,
    collect_days: i64,
    collect_from: Option<String>,
    collect_to: Option<String>,
    summarize_limit: usize,
    keep_days: i64,
    /// Where models are kept — resolved, so the window shows the path in use
    /// rather than the blank that means "wherever you normally put them".
    models_dir: String,
    /// What the database costs on disk right now, in megabytes. The answer to
    /// "does this grow forever" should be visible, not explained.
    database_megabytes: f64,
    sources: Vec<SourceDto>,
    config_path: String,
    articles: i64,
    summaries: i64,
    /// Readable, wanted, and not written up yet: what pressing summarize will
    /// go through.
    pending: i64,
    verdicts: Vec<VerdictDto>,
    busy: bool,
}

#[derive(Serialize)]
pub struct SourceDto {
    label: String,
    kind: String,
    lang: Option<String>,
    enabled: bool,
    /// Why this source did not answer last time, if it did not. The window
    /// marks the row and keeps the text for whoever wants to know what the
    /// server actually said.
    failure: Option<String>,
    /// The one thing that identifies this source beyond its name: the feed
    /// address, or the query an aggregator will run.
    detail: Option<String>,
}

#[derive(Serialize)]
pub struct VerdictDto {
    status: String,
    reason: Option<String>,
    count: i64,
}

#[derive(Serialize)]
pub struct CardDto {
    /// The summary's identity, which is what a recording is filed against and
    /// what the play button sends back. Omitting it from the payload — while
    /// the type behind it had the field — got as far as the window before
    /// failing, with the id arriving as null.
    id: i64,
    title: String,
    bullets: Vec<String>,
    tags: Vec<String>,
    relevance: f32,
    url: String,
    source: Option<String>,
    published: Option<String>,
}

/// A sentence the window will write, named here rather than written.
///
/// Only the key and its values cross the boundary; the wording and the plural
/// forms belong to the window's dictionaries.
#[derive(Clone, Serialize)]
pub struct Msg {
    code: &'static str,
    args: serde_json::Map<String, serde_json::Value>,
}

impl Msg {
    fn new(code: &'static str) -> Msg {
        Msg { code, args: serde_json::Map::new() }
    }

    fn with(mut self, key: &str, value: impl Into<serde_json::Value>) -> Msg {
        self.args.insert(key.to_string(), value.into());
        self
    }

    /// A failure, in the same shape as everything else the window is told.
    /// `detail` is the technical text, which is shown when the window has no
    /// sentence of its own for this code.
    fn from_error(e: &AppError) -> Msg {
        let msg = Msg::new(e.code());
        match e.detail() {
            Some(detail) => msg.with("detail", detail),
            None => msg,
        }
    }
}

#[derive(Clone, Serialize)]
struct Progress {
    phase: &'static str,
    message: Msg,
    done: u32,
    total: u32,
}

/// The outcome, as a list of statements rather than one string. Joining them
/// is the window's job, punctuation being a question about a language.
#[derive(Clone, Serialize)]
struct Finished {
    phase: &'static str,
    parts: Vec<Msg>,
    ok: bool,
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn cmd_overview(state: State<'_, AppState>) -> Result<Overview, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let store = Store::open(&paths::database_path()?)?;
    let hw = Hardware::probe();

    // A service has no file to find and nothing to download, so the rail names
    // it and offers no button.
    let service = config.settings.provider == crate::config::Provider::OpenAi;
    let resolved = match service {
        true => None,
        false => crate::cli::resolve_model(&config).ok(),
    };
    let suggested = registry::recommend(hw.vram_mb, hw.ram_mb);
    // Counted the way the feed counts: an article is written up when there is a
    // summary in the language being read, made by the prompt in use now.
    let lang = config.settings.output_lang;
    let version = crate::llm::prompt_version(Mode::TwoPass);
    // Which model would do the work, so the number promises only what pressing
    // the button will actually go through. Unknown when nothing is configured
    // yet, and then the count is of everything unwritten.
    let doer = crate::cli::model_id(&config).ok();
    // Counted over the current selection, so the numbers match the feed.
    let selection = Selection::from_config(&config);

    Ok(Overview {
        hardware: hw.describe(),
        model_name: match service {
            true => config.settings.api_model.clone(),
            false => resolved
                .as_ref()
                .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned())),
        },
        model_hint: (!service && resolved.is_none()).then(|| ModelHint {
            id: suggested.id.clone(),
            name: suggested.name.clone(),
            gigabytes: suggested.size_bytes as f64 / 1e9,
        }),
        model_loaded: state.engine.is_loaded(),
        ui_lang: config.settings.ui_lang.tag().to_string(),
        provider: config.settings.provider.as_str().to_string(),
        api_url: config.settings.api_url.clone(),
        api_model: config.settings.api_model.clone(),
        output_lang: config.settings.output_lang.code().to_string(),
        keywords: config.settings.keywords.clone(),
        speech_engine: config.settings.speech_engine.as_str().to_string(),
        speech_voice: config.settings.speech_voice.clone(),
        speech_rate: config.settings.speech_rate,
        audio_megabytes: {
            let (bytes, _) = store.audio_usage()?;
            bytes as f64 / 1e6
        },
        audio_files: store.audio_usage()?.1,
        collect_days: config.settings.collect_days,
        collect_from: config.settings.collect_from.clone(),
        collect_to: config.settings.collect_to.clone(),
        summarize_limit: config.settings.summarize_limit,
        keep_days: config.settings.keep_days,
        models_dir: paths::models_dir_or(config.settings.models_dir.as_deref())?
            .display()
            .to_string(),
        database_megabytes: std::fs::metadata(paths::database_path()?)
            .map(|m| m.len() as f64 / 1e6)
            .unwrap_or(0.0),
        sources: config
            .sources
            .iter()
            .map(|s| {
                // The same key the collection run files its answer under: the
                // address, or the name when a source has no address of its own.
                let key = s.url.clone().unwrap_or_else(|| s.label());
                SourceDto {
                    label: s.label(),
                    kind: format!("{:?}", s.kind).to_lowercase(),
                    lang: s.lang.map(|l| l.code().to_string()),
                    enabled: s.enabled,
                    failure: store.source_failure(&key).unwrap_or(None),
                    detail: s.url.clone(),
                }
            })
            .collect(),
        config_path: paths::sources_path()?.display().to_string(),
        articles: store.count_articles(&selection)?,
        summaries: store.count_summaries(lang, &version, &selection)?,
        pending: store.count_pending(lang, &version, doer.as_deref(), &selection)?,
        verdicts: store
            .verdict_breakdown(&selection)?
            .into_iter()
            .map(|(status, reason, count)| VerdictDto { status, reason, count })
            // Articles a service would not take. Not a verdict on the article —
            // it passed every gate — so it is listed apart from them,
            // under a status of its own.
            .chain(store.refusal_breakdown(&selection)?.into_iter().map(|(_, detail, count)| {
                VerdictDto { status: "refused".into(), reason: detail, count }
            }))
            .collect(),
        busy: state.busy.load(Ordering::SeqCst),
    })
}

/// The feed, narrowed to what the reader is looking for.
///
/// `query` searches what is already stored, not the sources. It goes to the
/// database rather than filtering the cards the window is holding, or a word
/// past the loaded page would not be findable.
#[tauri::command]
fn cmd_cards(
    limit: Option<usize>,
    skip: Option<usize>,
    query: Option<String>,
) -> Result<Vec<CardDto>, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let store = Store::open(&paths::database_path()?)?;

    // Read from the configuration, never passed in: a copy the window keeps
    // eventually disagrees with the one on disk.
    let target = config.settings.output_lang;
    let version = crate::llm::prompt_version(Mode::TwoPass);

    Ok(store
        .recent_summaries(
            target,
            &version,
            limit.unwrap_or(30),
            skip.unwrap_or(0),
            &Selection::from_config(&config)
                .searching(query.as_deref().unwrap_or_default(), target),
        )?
        .into_iter()
        .map(|c| CardDto {
            id: c.id,
            title: c.title,
            bullets: c.bullets,
            tags: c.tags,
            relevance: c.relevance,
            url: c.url,
            source: c.source_label,
            published: c.published_at.map(|d| d.format("%Y-%m-%d %H:%M").to_string()),
        })
        .collect())
}

#[tauri::command]
fn cmd_collect(app: AppHandle, state: State<'_, AppState>) -> Result<(), AppError> {
    let Some(cancel) = state.claim() else {
        return Err(AppError::told("busy", "another job is already running"));
    };

    spawn_job(app, "collect", move |app| {
        // Its own runtime, its own connection: neither ever leaves this thread.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AppError::Other(e.to_string()))?;

        let config = Config::load_or_init(&paths::sources_path()?)?;
        let store = Store::open(&paths::database_path()?)?;
        let fetcher = Fetcher::new()?;
        let range = config.date_range();

        emit_progress(&app, "collect", Msg::new("job.polling"), 0, 0);
        let report = runtime.block_on(pipeline::collect(
            &config,
            &store,
            &fetcher,
            range,
            &cancel,
            |stage, done, total| {
                let code = match stage {
                    pipeline::Stage::Polling => "job.polling",
                    pipeline::Stage::Reading => "job.reading",
                };
                emit_progress(&app, "collect", Msg::new(code), done as u32, total as u32);
            },
        ))?;
        store.checkpoint()?;

        // The keyword count is reported separately, and only when there were
        // keywords: one number for "отсеяно 143" said nothing about which of
        // the two reasons was doing the work.
        let mut parts = vec![
            Msg::new("job.collect.polled").with("n", report.sources_polled),
            Msg::new("job.collect.stored").with("n", report.stored),
            Msg::new("job.collect.rejected").with("n", report.rejected),
            Msg::new("job.collect.duplicates").with("n", report.duplicates),
        ];
        // How far each source reached, naming them rather than counting: a
        // short list otherwise reads as a quiet week.
        if !report.sources_by_address.is_empty() {
            parts.push(
                Msg::new("job.collect.by_address")
                    .with("sources", report.sources_by_address.join(", ")),
            );
        }
        if !report.sources_feed_only.is_empty() {
            parts.push(
                Msg::new("job.collect.feed_only")
                    .with("sources", report.sources_feed_only.join(", ")),
            );
        }
        // Sources that had more to give than one run takes, with how much was
        // left behind.
        if !report.sources_cut.is_empty() {
            let cut: Vec<String> =
                report.sources_cut.iter().map(|(name, n)| format!("{name} +{n}")).collect();
            parts.push(Msg::new("job.collect.cut").with("sources", cut.join(", ")));
        }
        if report.sources_failed > 0 {
            parts.push(Msg::new("job.collect.failed").with("n", report.sources_failed));
        }
        if report.off_topic > 0 {
            parts.push(Msg::new("job.collect.off_topic").with("n", report.off_topic));
        }
        if report.pruned > 0 {
            parts.push(Msg::new("job.collect.pruned").with("n", report.pruned));
        }
        Ok(parts)
    });

    Ok(())
}

#[tauri::command]
fn cmd_summarize(app: AppHandle, state: State<'_, AppState>) -> Result<(), AppError> {
    let Some(cancel) = state.claim() else {
        return Err(AppError::told("busy", "another job is already running"));
    };
    let engine = state.engine.clone();
    let cancel_for_load = cancel.clone();

    spawn_job(app, "summarize", move |app| {
        let config = Config::load_or_init(&paths::sources_path()?)?;
        let store = Store::open(&paths::database_path()?)?;
        // Zero total means "length unknown", and the window sweeps the bar:
        // llama.cpp reports no progress while it reads the file.
        emit_progress(&app, "summarize", Msg::new("job.loading_model"), 0, 0);
        let model_id = crate::cli::prepare_engine(&engine, &config, &cancel_for_load)?;

        let report = pipeline::summarize_pending(
            &store,
            &engine,
            &model_id,
            config.settings.output_lang,
            &config.settings.keywords,
            Mode::TwoPass,
            config.settings.summarize_limit,
            &Selection::from_config(&config),
            &cancel,
            |title, done, total| {
                let title: String = title.chars().take(80).collect();
                let message = Msg::new("job.article").with("title", title);
                emit_progress(&app, "summarize", message, done as u32, total as u32);
            },
            // A batch is written down and shown before the next one starts, so
            // a long run fills the feed as it goes rather than all at the end.
            || {
                let _ = app.emit("job://batch", ());
            },
        );
        store.checkpoint()?;
        let report = report?;

        let mut parts = vec![
            Msg::new("job.summarize.done").with("n", report.summarized),
            Msg::new("job.summarize.not_article").with("n", report.not_an_article),
            Msg::new("job.summarize.failed").with("n", report.failed),
            Msg::new("job.summarize.refused").with("n", report.refused),
        ];
        // The counts say how many failed; this says why the last one did.
        if let Some(e) = &report.last_failure {
            parts.push(Msg::from_error(e));
        }
        Ok(parts)
    });

    Ok(())
}

/// What the window sends when settings change. Every field is optional so the
/// UI can save one thing without having to know the rest.
#[derive(Deserialize)]
pub struct SettingsPatch {
    ui_lang: Option<String>,
    provider: Option<String>,
    api_url: Option<String>,
    api_model: Option<String>,
    output_lang: Option<String>,
    keywords: Option<Vec<String>>,
    model: Option<String>,
    collect_days: Option<i64>,
    collect_from: Option<String>,
    collect_to: Option<String>,
    summarize_limit: Option<usize>,
    keep_days: Option<i64>,
    models_dir: Option<String>,
    speech_engine: Option<String>,
    speech_voice: Option<String>,
    speech_rate: Option<f32>,
}

#[tauri::command]
fn cmd_save_settings(patch: SettingsPatch) -> Result<(), AppError> {
    let path = paths::sources_path()?;
    let mut config = Config::load_or_init(&path)?;

    if let Some(tag) = patch.ui_lang {
        config.settings.ui_lang = tag.parse().map_err(AppError::Other)?;
    }
    if let Some(code) = patch.output_lang {
        config.settings.output_lang = code.parse::<Lang>().map_err(AppError::Other)?;
    }
    if let Some(words) = patch.keywords {
        config.settings.keywords =
            words.into_iter().map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect();
    }
    if let Some(provider) = patch.provider {
        config.settings.provider = provider.parse().map_err(AppError::Other)?;
    }
    // Blank means "not set", the same rule the model box follows. A service
    // with no key is a real configuration — a llama.cpp server on the next desk
    // wants none — so an empty key is stored as no key rather than refused.
    for (field, value) in [
        (&mut config.settings.api_url, patch.api_url),
        (&mut config.settings.api_model, patch.api_model),
    ] {
        if let Some(v) = value {
            *field = (!v.trim().is_empty()).then(|| v.trim().to_string());
        }
    }
    if let Some(model) = patch.model {
        // An empty box means "choose for me", not "the model named nothing".
        config.settings.model = (!model.trim().is_empty()).then(|| model.trim().to_string());
    }
    if let Some(days) = patch.collect_days {
        config.settings.collect_days = days.max(0);
    }
    // An empty box clears the date rather than storing a blank string, so the
    // rolling window takes over again the moment someone deletes what they typed.
    if let Some(from) = patch.collect_from {
        config.settings.collect_from = (!from.trim().is_empty()).then(|| from.trim().to_string());
    }
    if let Some(to) = patch.collect_to {
        config.settings.collect_to = (!to.trim().is_empty()).then(|| to.trim().to_string());
    }
    if let Some(limit) = patch.summarize_limit {
        config.settings.summarize_limit = limit.clamp(1, 500);
    }
    if let Some(days) = patch.keep_days {
        config.settings.keep_days = days.max(0);
    }
    // An empty box means "wherever you normally put them", not "the folder
    // named nothing" — the same rule the model box follows.
    if let Some(dir) = patch.models_dir {
        config.settings.models_dir = (!dir.trim().is_empty()).then(|| dir.trim().to_string());
    }

    if let Some(engine) = patch.speech_engine {
        config.settings.speech_engine = engine.parse().map_err(AppError::Other)?;
    }
    if let Some(voice) = patch.speech_voice {
        config.settings.speech_voice = (!voice.trim().is_empty()).then(|| voice.trim().to_string());
    }
    if let Some(rate) = patch.speech_rate {
        config.settings.speech_rate = rate.clamp(0.5, 2.0);
    }

    config.save(&path)
}

/// The form on the Sources screen, filled from a source and read back into
/// one. Validation lives in `Config`.
#[derive(Deserialize, Serialize)]
pub struct NewSource {
    kind: String,
    title: Option<String>,
    url: Option<String>,
    search_url: Option<String>,
    sitemap_url: Option<String>,
    lang: Option<String>,
    min_points: Option<i64>,
    follow_external: Option<bool>,
    links_selector: Option<String>,
    body_selector: Option<String>,
    items_path: Option<String>,
    item_url_path: Option<String>,
    item_title_path: Option<String>,
    item_date_path: Option<String>,
    item_score_path: Option<String>,
    #[serde(default)]
    headers: std::collections::HashMap<String, String>,
}

/// The source at `index`, in the shape the form takes.
#[tauri::command]
fn cmd_source(index: usize) -> Result<NewSource, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let spec = config
        .sources
        .get(index)
        .ok_or_else(|| AppError::told("no_such_source", "no source with that number"))?;

    let map = spec.map.as_ref();
    Ok(NewSource {
        kind: format!("{:?}", spec.kind).to_lowercase(),
        title: spec.title.clone(),
        url: spec.url.clone(),
        search_url: spec.search_url.clone(),
        sitemap_url: spec.sitemap_url.clone(),
        lang: spec.lang.map(|l| l.code().to_string()),
        min_points: spec.min_points,
        follow_external: Some(spec.follow_external),
        links_selector: spec.selectors.as_ref().map(|s| s.links.clone()),
        body_selector: spec.selectors.as_ref().and_then(|s| s.body.clone()),
        items_path: map.map(|m| m.items.clone()),
        item_url_path: map.map(|m| m.url.clone()),
        item_title_path: map.and_then(|m| m.title.clone()),
        item_date_path: map.and_then(|m| m.published.clone()),
        item_score_path: map.and_then(|m| m.score.clone()),
        headers: spec.headers.clone(),
    })
}

/// Write a source: over the one at `index`, or at the end when there is none.
#[tauri::command]
fn cmd_save_source(index: Option<usize>, source: NewSource) -> Result<(), AppError> {
    let path = paths::sources_path()?;
    let mut config = Config::load_or_init(&path)?;

    let kind = match source.kind.as_str() {
        "rss" => Kind::Rss,
        "html_list" => Kind::HtmlList,
        "json_api" => Kind::JsonApi,
        other => {
            return Err(AppError::told_about(
                "unknown_kind",
                format!("unknown source kind '{other}'"),
                other,
            ));
        }
    };

    let trimmed = |v: Option<String>| v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());

    let spec = SourceSpec {
        kind,
        title: trimmed(source.title),
        url: trimmed(source.url),
        // Not `trimmed`: an empty address here is a reader who cleared the
        // field on purpose, and it has to survive as an empty string. Dropped
        // to nothing it would be filled in again on the next start — and for
        // the map, "this publisher has none" would be re-checked every run.
        search_url: source.search_url.map(|s| s.trim().to_string()),
        sitemap_url: source.sitemap_url.map(|s| s.trim().to_string()),
        lang: match trimmed(source.lang) {
            Some(code) => Some(code.parse::<Lang>().map_err(AppError::Other)?),
            None => None,
        },
        enabled: true,
        min_points: source.min_points,
        follow_external: source.follow_external.unwrap_or(true),
        selectors: trimmed(source.links_selector).map(|links| Selectors {
            links,
            body: trimmed(source.body_selector),
            strip: Vec::new(),
        }),
        headers: source.headers,
        map: trimmed(source.item_url_path).map(|url| FieldMap {
            items: trimmed(source.items_path).unwrap_or_default(),
            url,
            title: trimmed(source.item_title_path),
            published: trimmed(source.item_date_path),
            score: trimmed(source.item_score_path),
            comments: None,
            discussion: None,
            discussion_template: None,
            url_from_discussion: false,
            excerpt: None,
        }),
    };

    match index {
        Some(i) => {
            if !config.replace_source(i, spec) {
                return Err(AppError::told("no_such_source", "no source with that number"));
            }
        }
        None => config.add_source(spec),
    }
    config.save(&path)
}

#[tauri::command]
fn cmd_remove_source(index: usize) -> Result<(), AppError> {
    let path = paths::sources_path()?;
    let mut config = Config::load_or_init(&path)?;
    if !config.remove_source(index) {
        return Err(AppError::told("no_such_source", "no source with that number"));
    }
    config.save(&path)
}

#[tauri::command]
fn cmd_toggle_source(index: usize, enabled: bool) -> Result<(), AppError> {
    let path = paths::sources_path()?;
    let mut config = Config::load_or_init(&path)?;
    if !config.set_enabled(index, enabled) {
        return Err(AppError::told("no_such_source", "no source with that number"));
    }
    config.save(&path)
}

#[derive(Serialize)]
pub struct ModelDto {
    id: String,
    name: String,
    tier: String,
    params_b: f32,
    quant: String,
    download_gb: f64,
    memory_gb: f64,
    present: bool,
    suggested: bool,
    /// True for the model the next run will load.
    selected: bool,
    /// True when the user chose this one explicitly rather than leaving it to the program.
    pinned: bool,
}

#[tauri::command]
fn cmd_models() -> Result<Vec<ModelDto>, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let dir = paths::models_dir_or(config.settings.models_dir.as_deref())?;
    let hw = Hardware::probe();
    let suggested = registry::recommend(hw.vram_mb, hw.ram_mb);
    // Which model the next run will really load. Marking only an explicit
    // setting left every row unmarked in the common case where the program
    // picks for itself — which tells the user nothing about what is in use.
    let local = config.settings.provider == crate::config::Provider::Local;
    let effective = crate::cli::resolve_model(&config)
        .ok()
        .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()));

    Ok(registry::catalog()
        .into_iter()
        .map(|m| ModelDto {
            present: dir.join(&m.filename).is_file(),
            suggested: m.id == suggested.id,
            // Only when the work is local. The two lists offer one choice
            // between them, and a model marked as chosen while a service is
            // writing the summaries would be describing something that is not
            // happening.
            selected: local && effective.as_deref() == Some(m.filename.as_str()),
            pinned: config.settings.model.as_deref() == Some(m.id.as_str()),
            tier: m.tier.label().to_string(),
            quant: m.quant.label().to_string(),
            download_gb: m.size_bytes as f64 / 1e9,
            memory_gb: m.memory_required_mb as f64 / 1024.0,
            id: m.id,
            name: m.name,
            params_b: m.params_b,
        })
        .collect())
}

#[tauri::command]
fn cmd_download_model(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), AppError> {
    let Some(cancel) = state.claim() else {
        return Err(AppError::told("busy", "another job is already running"));
    };
    let info = registry::by_id(&id).ok_or_else(|| {
        AppError::told_about("unknown_model", format!("no model '{id}' in the catalog"), &id)
    })?;

    spawn_job(app, "download", move |app| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AppError::Other(e.to_string()))?;
        let config = Config::load_or_init(&paths::sources_path()?)?;
        let dir = paths::ensure(paths::models_dir_or(config.settings.models_dir.as_deref())?)?;

        runtime.block_on(async {
            downloader::clean_partials(&dir).await?;
            downloader::download(
                &info.url,
                &dir,
                &info.filename,
                info.sha256.as_deref(),
                &cancel,
                |p| {
                    let code = match p.phase {
                        downloader::Phase::Downloading => "job.downloading",
                        downloader::Phase::Verifying => "job.verifying",
                    };
                    let message = Msg::new(code)
                        .with("name", info.name.clone())
                        .with("done", p.downloaded as f64 / 1e9)
                        .with("total", p.total as f64 / 1e9);
                    emit_progress(
                        &app,
                        "download",
                        with_speed(message, p),
                        p.percent() as u32,
                        100,
                    );
                },
            )
            .await
        })?;

        Ok(vec![Msg::new("job.model_ready").with("name", info.name)])
    });

    Ok(())
}

#[tauri::command]
fn cmd_delete_model(id: String) -> Result<(), AppError> {
    let info = registry::by_id(&id).ok_or_else(|| {
        AppError::told_about("unknown_model", format!("no model '{id}' in the catalog"), &id)
    })?;
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let path = paths::models_dir_or(config.settings.models_dir.as_deref())?.join(&info.filename);
    if path.is_file() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

/// Read one card aloud.
///
/// Off the main thread, synthesis taking seconds. The audio is played here
/// rather than handed to the window, an `<audio>` element on Linux needing a
/// GStreamer plugin the desktop may not have.
#[derive(Serialize)]
pub struct Speech {
    duration: f32,
    engine: String,
    voice: String,
    cached: bool,
}

#[tauri::command]
async fn cmd_speak(state: State<'_, AppState>, card: i64) -> Result<Speech, AppError> {
    let player = Some(state.player.clone());
    tauri::async_runtime::spawn_blocking(move || voice_over(card, player))
        .await
        .map_err(|e| AppError::Other(e.to_string()))?
}

/// Synthesize a card without playing it, so the next article in a queue is
/// ready before the current one ends.
///
/// The recording is written to disk and recorded in the database, so
/// `cmd_speak` finds it cached when its turn comes.
#[tauri::command]
async fn cmd_prepare(card: i64) -> Result<Speech, AppError> {
    tauri::async_runtime::spawn_blocking(move || voice_over(card, None))
        .await
        .map_err(|e| AppError::Other(e.to_string()))?
}

/// Synthesize a card, and play it if a player is given.
///
/// Shared by `cmd_speak` and `cmd_prepare`: everything but the last line is
/// the same work.
fn voice_over(
    card: i64,
    player: Option<Arc<crate::tts::player::Player>>,
) -> Result<Speech, AppError> {
    {
        let config = Config::load_or_init(&paths::sources_path()?)?;
        let store = Store::open(&paths::database_path()?)?;
        let target = config.settings.output_lang;
        let engine = config.settings.speech_engine;
        let rate = config.settings.speech_rate.clamp(0.5, 2.0);

        let voice_name = match (&config.settings.speech_voice, engine) {
            (Some(name), _) if !name.trim().is_empty() => name.trim().to_string(),
            (_, crate::tts::Engine::Online) => {
                crate::tts::online::default_voice(target).to_string()
            }
            (_, crate::tts::Engine::Local) => String::new(),
        };

        // Already made? Then nothing is synthesised, and with the online engine
        // nothing is sent anywhere either.
        if let Some((path, duration)) =
            store.cached_audio(card, engine.as_str(), &voice_name, rate)?
        {
            if let Some(player) = &player {
                player.play_file(card, &path, duration)?;
            }
            return Ok(Speech {
                duration,
                engine: engine.as_str().to_string(),
                voice: voice_name,
                cached: true,
            });
        }

        let text = store
            .summary_text(card)?
            .ok_or_else(|| AppError::told("summary_gone", "that summary no longer exists"))?;

        let voice = crate::tts::Voice::new();
        if engine == crate::tts::Engine::Local {
            let model = crate::cli::pick_voice(
                &paths::voices_dir()?,
                target,
                (!voice_name.is_empty()).then_some(voice_name.as_str()),
            )?;
            voice.load(&model)?;
        }

        let (samples, sample_rate) = crate::tts::speak(
            engine,
            &voice,
            &text,
            target,
            (!voice_name.is_empty()).then_some(voice_name.as_str()),
            rate,
        )?;
        voice.unload();

        let dir = paths::ensure(paths::audio_dir()?)?;
        let path = dir.join(format!("{card}-{}-{}.wav", engine.as_str(), sanitise(&voice_name)));
        crate::tts::voice::write_wav(&path, &samples, sample_rate)?;

        let duration = samples.len() as f32 / sample_rate as f32;
        store.save_audio(card, engine.as_str(), &voice_name, rate, &path, duration)?;

        if let Some(player) = &player {
            player.play(card, &samples, sample_rate)?;
        }
        Ok(Speech {
            duration,
            engine: engine.as_str().to_string(),
            voice: voice_name,
            cached: false,
        })
    }
}

/// Make a voice name safe to put in a file name.
fn sanitise(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_alphanumeric() { c } else { '-' }).collect();
    if cleaned.is_empty() { "default".into() } else { cleaned }
}

#[tauri::command]
fn cmd_playback(state: State<'_, AppState>, action: String) -> crate::tts::player::Playback {
    match action.as_str() {
        "pause" => state.player.pause(),
        "resume" => state.player.resume(),
        "stop" => state.player.stop(),
        _ => {}
    }
    state.player.state()
}

#[tauri::command]
fn cmd_playback_state(state: State<'_, AppState>) -> crate::tts::player::Playback {
    state.player.state()
}

/// Re-run the quality gates over everything already on disk.
///
/// Costs no network and no inference. Rejected articles are allowed back in, and
/// a re-judged article is judged on its stored text alone, so the checks that
/// need the page's markup cannot run.
#[tauri::command]
fn cmd_recheck() -> Result<Msg, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let store = Store::open(&paths::database_path()?)?;
    let cancel = crate::shutdown::Cancel::new();

    let report = pipeline::recheck(&config, &store, true, &cancel);
    store.checkpoint()?;
    let report = report?;

    if report.changed == 0 {
        return Ok(Msg::new("recheck.nothing").with("n", report.examined));
    }
    Ok(Msg::new("recheck.done")
        .with("examined", report.examined)
        .with("changed", report.changed)
        .with("dropped", report.summaries_dropped))
}

#[tauri::command]
fn cmd_prune() -> Result<Msg, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let path = paths::database_path()?;
    let store = Store::open(&path)?;

    let size = || std::fs::metadata(&path).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    let was = size();

    let Some(cutoff) = config.prune_before() else {
        return Ok(Msg::new("prune.no_limit"));
    };

    let report = store.prune(cutoff)?;
    store.compact()?;

    // Zero is the usual answer for a young database, and "забыто 0" reads like
    // a failure. Say what the threshold actually is instead.
    if report.articles == 0 {
        // The date goes over as a date, not as text: 03.09 and 09/03 are the
        // same day written by two people who would each misread the other.
        return Ok(Msg::new("prune.nothing")
            .with("cutoff", cutoff.to_rfc3339())
            .with("size", size()));
    }

    Ok(Msg::new("prune.done")
        .with("articles", report.articles)
        .with("summaries", report.summaries)
        .with("recordings", report.recordings)
        .with("size", size())
        .with("was", was))
}

/// Throw the whole cache away. Everything here can be collected again.
#[tauri::command]
fn cmd_clear_all() -> Result<Msg, AppError> {
    let path = paths::database_path()?;
    let store = Store::open(&path)?;
    let size = || std::fs::metadata(&path).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    let was = size();

    let report = store.clear_all()?;
    store.compact()?;
    Ok(Msg::new("clear.done")
        .with("articles", report.articles)
        .with("summaries", report.summaries)
        .with("recordings", report.recordings)
        .with("size", size())
        .with("was", was))
}

/// One saved service, as the window lists it.
#[derive(Serialize)]
pub struct ServiceDto {
    url: String,
    model: String,
    /// Whether a key is stored. The key itself does not travel: the window has
    /// no use for it, and the one place it is needed is the request.
    has_key: bool,
    /// True for the service currently writing the summaries.
    selected: bool,
}

#[tauri::command]
fn cmd_services() -> Result<Vec<ServiceDto>, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let chosen = config.chosen_service().cloned();
    Ok(config
        .services
        .iter()
        .map(|s| ServiceDto {
            selected: chosen.as_ref().is_some_and(|c| c.is(&s.url, &s.model)),
            has_key: s.key.is_some(),
            url: s.url.clone(),
            model: s.model.clone(),
        })
        .collect())
}

/// Add a service to the list, or correct the one already there.
///
/// Saving does not select it: choosing which model writes the summaries is its
/// own act, in the list above the form.
#[tauri::command]
fn cmd_save_service(url: String, model: String, key: Option<String>) -> Result<(), AppError> {
    let path = paths::sources_path()?;
    let mut config = Config::load_or_init(&path)?;

    // Checked here rather than at the moment of use: a service with no address
    // is not something to store and discover later.
    let service = crate::config::Service {
        url: url.trim().to_string(),
        model: model.trim().to_string(),
        key: key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()),
    };
    if service.url.is_empty() {
        return Err(AppError::told("api_no_url", "no address configured for the service"));
    }
    if service.model.is_empty() {
        return Err(AppError::told("api_no_model", "no model name configured for the service"));
    }

    config.save_service(service);
    config.save(&path)
}

#[tauri::command]
fn cmd_remove_service(index: usize) -> Result<(), AppError> {
    let path = paths::sources_path()?;
    let mut config = Config::load_or_init(&path)?;
    if !config.remove_service(index) {
        return Err(AppError::told("no_such_service", "no service with that number"));
    }
    config.save(&path)
}

/// Ask the service one trivial question, to find out whether it answers.
///
/// Takes the address, model and key as they stand on the screen rather than
/// on disk: checking happens before saving, so these are candidate values.
/// Takes no job lock — it is one short request.
#[tauri::command]
async fn cmd_check_service(
    url: String,
    model: String,
    key: Option<String>,
) -> Result<Msg, AppError> {
    tauri::async_runtime::spawn_blocking(move || {
        // No key given means "the one already saved for this service": the list
        // does not hand the key to the window, so a check from the list has to
        // find it here.
        let key = match key.map(|k| k.trim().to_string()).filter(|k| !k.is_empty()) {
            Some(typed) => Some(typed),
            None => Config::load_or_init(&paths::sources_path()?)?
                .services
                .iter()
                .find(|s| s.is(url.trim(), model.trim()))
                .and_then(|s| s.key.clone()),
        };
        let service = crate::llm::remote::Remote::new(&url, key.as_deref(), &model)?;

        // Short, but not so short that a reasoning model runs out of room
        // before it says anything: those spend their first tokens thinking, and
        // sixteen of them bought silence and a puzzling error.
        let opts = crate::llm::GenOptions { max_new_tokens: 256, ..Default::default() };
        let answer = service.generate(
            "Answer with one word.",
            "Say OK.",
            &opts,
            &crate::shutdown::Cancel::new(),
        )?;
        Ok(Msg::new("service.answered")
            .with("model", model)
            .with("said", answer.trim().chars().take(60).collect::<String>()))
    })
    .await
    .map_err(|e| AppError::Other(e.to_string()))?
}

/// Ask the system for a folder, and give back what was chosen.
///
/// Opened from Rust rather than granted to the window, so the page reaches
/// nothing directly. `None` means the dialog was closed without choosing.
#[tauri::command]
async fn cmd_choose_folder(
    app: AppHandle,
    start: Option<String>,
) -> Result<Option<String>, AppError> {
    let mut dialog = app.dialog().file();
    // Opening where the files are now saves the reader from navigating there.
    if let Some(dir) = start.filter(|d| !d.trim().is_empty()) {
        dialog = dialog.set_directory(dir);
    }

    let (tx, rx) = tokio::sync::oneshot::channel();
    dialog.pick_folder(move |picked| {
        let _ = tx.send(picked);
    });
    let picked = rx
        .await
        .map_err(|_| AppError::told("dialog_gone", "the folder chooser closed unexpectedly"))?;
    Ok(picked.map(|path| path.to_string()))
}

/// Hand `url` to the system browser.
///
/// http and https only. The address comes out of a feed, so the check belongs
/// here rather than in the window, which is what an article can reach.
#[tauri::command]
fn cmd_open_url(url: String) -> Result<(), AppError> {
    if !is_web_url(&url) {
        return Err(AppError::told_about(
            "not_a_web_link",
            format!("refusing to open '{url}'"),
            &url,
        ));
    }
    // `open` rather than a hand-rolled `xdg-open`: on Windows it hands the
    // address to PowerShell through an environment variable instead of pasting
    // it into a command line, so an `&` in a news URL cannot become a command
    // separator. Detached, or quitting the reader would take the browser with
    // it.
    open::that_detached(&url).map_err(|e| AppError::Other(format!("could not open {url}: {e}")))
}

/// True for an address that belongs in a browser.
fn is_web_url(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

#[tauri::command]
fn cmd_clear_audio() -> Result<i64, AppError> {
    let store = Store::open(&paths::database_path()?)?;
    store.clear_audio()
}

/// A voice the user can pick, and whether it is here yet.
#[derive(Serialize)]
pub struct VoiceOption {
    id: String,
    name: String,
    quality: String,
    /// Download size in gigabytes; zero for an online voice, which has none.
    size_gb: f64,
    present: bool,
    selected: bool,
}

/// Every voice worth offering for the current language and engine.
///
/// Both kinds in one call, because from the settings screen they are the same
/// question — who reads this — and only the answer's cost differs.
#[tauri::command]
fn cmd_voices() -> Result<Vec<VoiceOption>, AppError> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let lang = config.settings.output_lang;
    let chosen = config.settings.speech_voice.clone().unwrap_or_default();

    match config.settings.speech_engine {
        crate::tts::Engine::Local => {
            let dir = paths::voices_dir()?;
            Ok(crate::tts::catalogue::for_lang(lang)
                .into_iter()
                .map(|v| VoiceOption {
                    present: crate::tts::catalogue::is_installed(&dir, &v.id),
                    selected: chosen == v.id,
                    size_gb: v.size_bytes as f64 / 1e9,
                    id: v.id,
                    name: v.name,
                    quality: v.quality,
                })
                .collect())
        }
        crate::tts::Engine::Online => {
            let prefix = format!("{}-", lang.code());
            Ok(crate::tts::online::available_voices()?
                .into_iter()
                .filter(|(id, _)| id.to_lowercase().starts_with(&prefix))
                .map(|(id, label)| VoiceOption {
                    selected: chosen == id,
                    // Nothing to download: it is produced on their machines.
                    present: true,
                    size_gb: 0.0,
                    quality: String::new(),
                    name: label,
                    id,
                })
                .collect())
        }
    }
}

#[tauri::command]
fn cmd_download_voice(
    app: AppHandle,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), AppError> {
    let Some(cancel) = state.claim() else {
        return Err(AppError::told("busy", "another job is already running"));
    };
    let info = crate::tts::catalogue::by_id(&id).ok_or_else(|| {
        AppError::told_about("unknown_voice", format!("no voice '{id}' in the catalog"), &id)
    })?;

    spawn_job(app, "download", move |app| {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| AppError::Other(e.to_string()))?;
        let dir = paths::ensure(paths::voices_dir()?)?;

        runtime.block_on(async {
            // A voice is two files, and one without the other cannot load —
            // so the settings file is fetched first, being the small one, and
            // its absence is noticed before a hundred megabytes are spent.
            downloader::download(
                &info.config_url(),
                &dir,
                &format!("{}.onnx.json", info.id),
                None,
                &cancel,
                |_| {},
            )
            .await?;

            downloader::download(&info.model_url(), &dir, &info.filename(), None, &cancel, |p| {
                let message = Msg::new("job.voice_downloading")
                    .with("name", info.name.clone())
                    .with("done", p.downloaded as f64 / 1e6)
                    .with("total", p.total as f64 / 1e6);
                emit_progress(&app, "download", with_speed(message, p), p.percent() as u32, 100);
            })
            .await
        })?;

        // A Russian voice without the stress dictionary reads the news with
        // the accent in the wrong place about a third of the time. It is
        // twenty-one megabytes against the voice's sixty-three, so it comes
        // with the voice rather than being offered as a decision.
        if info.lang == crate::llm::Lang::Ru
            && !crate::tts::stress::StressDictionary::is_installed(&dir)
        {
            install_stress_dictionary(&app, &dir, &cancel)?;
        }

        Ok(vec![Msg::new("job.voice_ready").with("name", info.name)])
    });

    Ok(())
}

#[tauri::command]
fn cmd_cancel(state: State<'_, AppState>) {
    if let Some(cancel) = state.current.lock().as_ref() {
        cancel.cancel();
    }
}

/// Release the model's memory without quitting. Offered because holding several
/// gigabytes while the user reads is rude on a machine that has other work.
#[tauri::command]
fn cmd_unload(state: State<'_, AppState>) -> bool {
    state.engine.unload()
}

// ---------------------------------------------------------------------------
// plumbing
// ---------------------------------------------------------------------------

/// Run `work` on its own thread, reporting the outcome to the window.
///
/// The `WorkerGuard` is what makes quitting deterministic: `teardown` waits for
/// it, so a job in flight finishes unwinding before the model is dropped.
fn spawn_job<F>(app: AppHandle, phase: &'static str, work: F)
where
    F: FnOnce(AppHandle) -> AppResult<Vec<Msg>> + Send + 'static,
{
    std::thread::Builder::new()
        .name(format!("job-{phase}"))
        .spawn(move || {
            let _guard = Shutdown::global().worker();
            let result = work(app.clone());

            let finished = match result {
                Ok(parts) => Finished { phase, parts, ok: true },
                Err(AppError::Cancelled) => {
                    Finished { phase, parts: vec![Msg::new("job.cancelled")], ok: true }
                }
                Err(e) => {
                    tracing::warn!("{phase} failed: {e}");
                    Finished { phase, parts: vec![Msg::from_error(&e)], ok: false }
                }
            };

            if let Some(state) = app.try_state::<AppState>() {
                state.release();
            }
            let _ = app.emit("job://done", finished);
        })
        .ok();
}

/// Fetch RUAccent's dictionaries and turn them into the tables `stress` maps.
///
/// The archives are deleted afterwards; nothing reads them once the tables
/// exist.
fn install_stress_dictionary(
    app: &AppHandle,
    dir: &std::path::Path,
    cancel: &Cancel,
) -> AppResult<()> {
    use crate::tts::stress;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| AppError::Other(e.to_string()))?;

    let fetch = |url: &'static str, name: &'static str, total: u64| {
        let app = app.clone();
        async move {
            downloader::download(url, dir, name, None, cancel, |p| {
                let message = Msg::new("job.stress")
                    .with("done", p.downloaded as f64 / 1e6)
                    .with("total", total as f64 / 1e6);
                emit_progress(&app, "download", with_speed(message, p), p.percent() as u32, 100);
            })
            .await
        }
    };

    let (accents, yo) = runtime.block_on(async {
        let accents = fetch(stress::ACCENTS_URL, "accents.json.gz", stress::DOWNLOAD_BYTES).await?;
        let yo = fetch(stress::YO_URL, "yo_words.json.gz", stress::DOWNLOAD_BYTES).await?;
        Ok::<_, AppError>((accents, yo))
    })?;

    // Three million entries turned into two sorted tables, which takes a
    // moment and has no progress of its own to report.
    emit_progress(app, "download", Msg::new("job.stress_building"), 0, 0);
    stress::install(&accents, &yo, dir)?;
    let _ = std::fs::remove_file(&accents);
    let _ = std::fs::remove_file(&yo);
    Ok(())
}

fn with_speed(message: Msg, progress: downloader::Progress) -> Msg {
    match progress.bytes_per_second {
        0 => message,
        rate => message.with("speed", rate as f64 / 1e6),
    }
}

fn emit_progress(app: &AppHandle, phase: &'static str, message: Msg, done: u32, total: u32) {
    let _ = app.emit("job://progress", Progress { phase, message, done, total });
}

// ---------------------------------------------------------------------------
// entry point
// ---------------------------------------------------------------------------

pub fn run() {
    // Wayland takes the window icon from a .desktop file matched against the
    // application id, which Tauri sets only when enableGtkAppId is on. See
    // packaging/install-dev-desktop.sh for a build run out of target/.

    // On WebKitGTK the webview's accelerated renderer competes with the app's own
    // GPU work for the same device, and the window's content freezes during
    // inference bursts even though nothing heavy runs on the main thread.
    // Taking the webview off that path keeps the UI responsive under load.
    // A no-op on Windows and macOS; a user's own setting wins.
    #[cfg(target_os = "linux")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        unsafe { std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1") };
    }

    tauri::Builder::default()
        // Only for the folder chooser, and only ever opened from Rust: the
        // window is given no dialog permission of its own.
        .plugin(tauri_plugin_dialog::init())
        .manage(AppState::new())
        .setup(|app| {
            watch_for_shutdown(app.handle().clone());
            // Linux and WebKitGTK only: lay the page out again after the window
            // moves to a screen with a different scale, or it goes on drawing
            // for the screen it left.
            crate::monitor_fix::install(app.handle());
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            cmd_overview,
            cmd_cards,
            cmd_collect,
            cmd_summarize,
            cmd_cancel,
            cmd_unload,
            cmd_save_settings,
            cmd_save_source,
            cmd_source,
            cmd_remove_source,
            cmd_toggle_source,
            cmd_models,
            cmd_download_model,
            cmd_delete_model,
            cmd_speak,
            cmd_prepare,
            cmd_playback,
            cmd_playback_state,
            cmd_clear_audio,
            cmd_recheck,
            cmd_prune,
            cmd_clear_all,
            cmd_voices,
            cmd_download_voice,
            cmd_open_url,
            cmd_choose_folder,
            cmd_check_service,
            cmd_services,
            cmd_save_service,
            cmd_remove_service,
        ])
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { .. } = event {
                // Closing the window means quitting, and quitting means the
                // model is released here rather than left to the kernel.
                Shutdown::global().request();
                if let Some(state) = window.try_state::<AppState>() {
                    if let Some(cancel) = state.current.lock().as_ref() {
                        cancel.cancel();
                    }
                    // Silence before the window goes: audio that outlives its
                    // window is the background noise this whole design avoids.
                    state.player.stop();
                }
            }
        })
        .run(tauri::generate_context!())
        .expect("failed to start the window");
}

/// Turn the shutdown flag into the `exit` that stops the event loop.
///
/// Workers poll the flag, but the event loop does not, so a `SIGTERM` to the
/// GUI would otherwise leave the process running. Polled rather than signalled
/// over a channel because a signal handler must not allocate.
fn watch_for_shutdown(app: AppHandle) {
    std::thread::Builder::new()
        .name("shutdown-watch".into())
        .spawn(move || {
            while !Shutdown::global().is_requested() {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            tracing::info!("shutdown observed, closing the window");
            if let Some(state) = app.try_state::<AppState>()
                && let Some(cancel) = state.current.lock().as_ref()
            {
                cancel.cancel();
            }
            app.exit(0);
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_web_address_is_handed_to_the_system() {
        assert!(is_web_url("https://meduza.io/feature/2026/09/05/x"));
        assert!(is_web_url("http://example.com/a?b=1&c=2"));
        // These arrive from the same place a good link does — a feed — and the
        // system would act on every one of them.
        assert!(!is_web_url("file:///etc/passwd"));
        assert!(!is_web_url("javascript:alert(1)"));
        assert!(!is_web_url("mailto:someone@example.com"));
        assert!(!is_web_url("/etc/passwd"));
        assert!(!is_web_url(""));
    }

    #[test]
    fn a_voice_name_becomes_a_usable_file_name() {
        assert_eq!(sanitise("ru-RU-SvetlanaNeural"), "ru-RU-SvetlanaNeural");
        assert_eq!(sanitise("../../etc/passwd"), "------etc-passwd");
        assert_eq!(sanitise(""), "default");
    }
}
