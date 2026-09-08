// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! User configuration: sources, settings and services, in a TOML file meant to
//! be edited and copied between machines.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};
use crate::llm::Lang;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub settings: Settings,
    #[serde(default, rename = "source")]
    pub sources: Vec<SourceSpec>,
    /// Services the reader has saved. One of them may be the one writing the
    /// summaries; the rest sit in the list waiting to be picked.
    #[serde(default, rename = "service")]
    pub services: Vec<Service>,
}

/// One saved service: where it is, what it calls its model, and the key it
/// wants. Held as a list because a reader keeps more than one — a paid account
/// for good summaries and a server on the next desk for the rest — and
/// switching between them should not mean typing an address again.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Service {
    pub url: String,
    pub model: String,
    #[serde(default)]
    pub key: Option<String>,
}

impl Service {
    /// Whether this is the entry the settings are pointing at. The pair of
    /// address and model is the identity: an index would move when something
    /// above it is deleted.
    pub fn is(&self, url: &str, model: &str) -> bool {
        self.url == url && self.model == model
    }
}

/// Who writes the summaries: a model on this machine, or an external service.
///
/// With `OpenAi` the article text leaves the machine, so every screen that
/// offers it says so.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// A GGUF running in this process.
    #[default]
    Local,
    /// Any service speaking the OpenAI chat-completions shape: OpenAI,
    /// OpenRouter, LM Studio, Ollama, a llama.cpp server.
    ///
    /// Renamed for serde: the derived `open_ai` is not what anyone types into
    /// the file by hand.
    #[serde(rename = "openai")]
    OpenAi,
}

impl Provider {
    pub fn as_str(self) -> &'static str {
        match self {
            Provider::Local => "local",
            Provider::OpenAi => "openai",
        }
    }
}

impl std::str::FromStr for Provider {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" | "gguf" => Ok(Provider::Local),
            "openai" | "api" | "remote" => Ok(Provider::OpenAi),
            other => Err(format!("unknown model provider '{other}' (expected local or openai)")),
        }
    }
}

/// The language the window speaks.
///
/// Separate from `Lang`: the interface is translated into more languages than
/// the model writes summaries in, and the two are chosen independently.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum UiLang {
    #[default]
    #[serde(rename = "en")]
    En,
    #[serde(rename = "de")]
    De,
    #[serde(rename = "es")]
    Es,
    #[serde(rename = "fr")]
    Fr,
    #[serde(rename = "pt-BR")]
    PtBr,
    #[serde(rename = "ru")]
    Ru,
    #[serde(rename = "zh-CN")]
    ZhCn,
}

impl UiLang {
    pub const ALL: [UiLang; 7] =
        [UiLang::En, UiLang::De, UiLang::Es, UiLang::Fr, UiLang::PtBr, UiLang::Ru, UiLang::ZhCn];

    /// The BCP 47 tag, which is also what the window uses to pick a dictionary
    /// and what goes into the page's `lang` attribute.
    pub fn tag(self) -> &'static str {
        match self {
            UiLang::En => "en",
            UiLang::De => "de",
            UiLang::Es => "es",
            UiLang::Fr => "fr",
            UiLang::PtBr => "pt-BR",
            UiLang::Ru => "ru",
            UiLang::ZhCn => "zh-CN",
        }
    }
}

impl std::str::FromStr for UiLang {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        UiLang::ALL
            .into_iter()
            .find(|l| l.tag().eq_ignore_ascii_case(s.trim()))
            .ok_or_else(|| format!("unknown interface language '{s}'"))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Language of the window itself: buttons, labels and explanations.
    #[serde(default)]
    pub ui_lang: UiLang,
    /// Language every summary is written in.
    #[serde(default = "default_output_lang")]
    pub output_lang: Lang,
    /// Words the reader cares about. Empty means "everything".
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Model id from the catalog, or an absolute path to a GGUF.
    #[serde(default)]
    pub model: Option<String>,
    /// Who writes the summaries: the model on this machine, or a service.
    #[serde(default)]
    pub provider: Provider,
    /// The service's address, as its documentation gives it — the base, such as
    /// `https://api.openai.com/v1`. `/chat/completions` is added unless the
    /// address already ends in it, because half the world's documentation
    /// prints the base and the other half prints the endpoint.
    #[serde(default)]
    pub api_url: Option<String>,
    /// What the service calls the model: "gpt-4o-mini", "qwen2.5:14b".
    ///
    /// With `api_url`, names which saved service is in use. The key is not
    /// repeated here; it stays with the `Service` entry.
    #[serde(default)]
    pub api_model: Option<String>,
    /// Where downloaded models are kept. Empty means beside the database.
    #[serde(default)]
    pub models_dir: Option<String>,
    /// How far back a collection run looks, in days. Zero means no limit.
    ///
    /// Ignored when `collect_from` or `collect_to` is set.
    #[serde(default = "default_collect_days")]
    pub collect_days: i64,
    /// Start of an explicit window, as YYYY-MM-DD.
    #[serde(default)]
    pub collect_from: Option<String>,
    /// End of an explicit window, as YYYY-MM-DD. Inclusive: the whole day counts.
    #[serde(default)]
    pub collect_to: Option<String>,
    /// How many articles one summarize run will work through.
    #[serde(default = "default_summarize_limit")]
    pub summarize_limit: usize,
    /// Delete stored articles older than this many days. Zero keeps everything.
    #[serde(default = "default_keep_days")]
    pub keep_days: i64,

    /// Which engine reads the summaries aloud. Local by default: the online
    /// one sends the summary text to a third party.
    #[serde(default)]
    pub speech_engine: crate::tts::Engine,
    /// Voice name, meaning whatever the chosen engine calls a voice: "irina"
    /// for the local one, "ru-RU-SvetlanaNeural" for the online one. Empty
    /// leaves the choice to the program.
    #[serde(default)]
    pub speech_voice: Option<String>,
    /// Speaking pace, where 1.0 is the voice's own. The voices disagree about
    /// what their own is, so this is per-installation rather than per-voice.
    #[serde(default = "default_speech_rate")]
    pub speech_rate: f32,
}

/// English, to agree with the interface's own default. A reader who wants
/// something else says so once, in the settings or in this file.
fn default_output_lang() -> Lang {
    Lang::En
}

fn default_speech_rate() -> f32 {
    1.0
}

fn default_collect_days() -> i64 {
    3
}

fn default_summarize_limit() -> usize {
    30
}

fn default_keep_days() -> i64 {
    90
}

impl Settings {
    /// The words a searchable source should be asked about, one per entry.
    ///
    /// Separate entries rather than one string because services disagree about
    /// how to spell "either of these"; asking once per term works everywhere.
    /// Exclusions are left out — they are applied locally by `KeywordFilter`.
    pub fn search_terms(&self) -> Vec<String> {
        self.keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty() && !k.starts_with('-'))
            .map(str::to_string)
            .collect()
    }
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            ui_lang: UiLang::default(),
            output_lang: default_output_lang(),
            keywords: Vec::new(),
            model: None,
            provider: Provider::default(),
            api_url: None,
            api_model: None,
            models_dir: None,
            collect_days: default_collect_days(),
            collect_from: None,
            collect_to: None,
            summarize_limit: default_summarize_limit(),
            keep_days: default_keep_days(),
            speech_engine: crate::tts::Engine::default(),
            speech_voice: None,
            speech_rate: default_speech_rate(),
        }
    }
}

/// One configured source. `kind` picks the adapter; the rest is adapter-specific
/// and ignored where it does not apply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceSpec {
    pub kind: Kind,
    #[serde(default)]
    pub title: Option<String>,
    /// Feed or listing URL. Unused by aggregators, which know their own endpoint.
    #[serde(default)]
    pub url: Option<String>,
    /// Address that can be asked a question, carrying `{query}`.
    ///
    /// Absent means this source cannot be searched; its map and feed answer
    /// for it instead.
    #[serde(default)]
    pub search_url: Option<String>,
    /// Address of the publisher's sitemap.
    ///
    /// Absent means look it up in `robots.txt`; an address here is used
    /// instead of looking; an empty string means this publisher has none and
    /// the search stops on every run.
    #[serde(default)]
    pub sitemap_url: Option<String>,
    /// Language the source publishes in, when known. Used by the quality gate
    /// to notice a consent page was fetched instead of an article.
    #[serde(default)]
    pub lang: Option<Lang>,
    #[serde(default = "default_true")]
    pub enabled: bool,

    // --- aggregator options ---
    /// Ignore aggregator items below this score.
    #[serde(default)]
    pub min_points: Option<i64>,
    /// Follow the source's outbound link and extract the real article. Off
    /// means only what the source itself gave is kept.
    #[serde(default = "default_true")]
    pub follow_external: bool,

    // --- html_list options ---
    #[serde(default)]
    pub selectors: Option<Selectors>,

    // --- json_api options ---
    #[serde(default)]
    pub map: Option<FieldMap>,
    /// Extra request headers, for an API that wants a token.
    ///
    /// Treated as secrets: never logged, and sent only in the request they
    /// belong to.
    #[serde(default)]
    pub headers: std::collections::HashMap<String, String>,
}

/// Where the interesting values live inside one service's JSON.
///
/// Paths are dotted: `data.children` walks into nested objects. A path may be
/// empty where the value is the object itself.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FieldMap {
    /// Path to the array of items. Empty when the response *is* the array.
    #[serde(default)]
    pub items: String,
    /// Path to the article's address. The one field with no sensible default.
    pub url: String,
    #[serde(default)]
    pub title: Option<String>,
    /// ISO 8601 text or a Unix timestamp; both are recognized.
    #[serde(default)]
    pub published: Option<String>,
    #[serde(default)]
    pub score: Option<String>,
    #[serde(default)]
    pub comments: Option<String>,
    /// Where to discuss it, when the service has such a page.
    #[serde(default)]
    pub discussion: Option<String>,
    /// Build the discussion address from the item's own fields, when the
    /// service gives an id rather than a link: `https://news.ycombinator.com/item?id={objectID}`.
    #[serde(default)]
    pub discussion_template: Option<String>,
    /// Keep an item that has no outbound link, pointing it at its discussion.
    ///
    /// Ask HN and Show HN posts carry their text and no link; skipping them
    /// silently loses a whole category of what the service publishes.
    #[serde(default)]
    pub url_from_discussion: bool,
    /// Where the item's own text lives, for services that publish posts rather
    /// than links to elsewhere.
    #[serde(default)]
    pub excerpt: Option<String>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Selectors {
    /// CSS selector matching links to articles on a listing page.
    pub links: String,
    /// Optional selector for the article body, when generic extraction fails.
    #[serde(default)]
    pub body: Option<String>,
    /// Elements to delete before extraction.
    #[serde(default)]
    pub strip: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// How to reach a source.
///
/// Named services do not belong here: one that cannot be expressed by these
/// three is a gap in the mechanism, and the fix belongs in the mechanism.
pub enum Kind {
    /// RSS, Atom or JSON Feed.
    Rss,
    /// A listing page scraped for links, for a site with no feed.
    HtmlList,
    /// Any service that answers with JSON. See [`FieldMap`].
    JsonApi,
}

impl Config {
    pub fn load(path: &Path) -> AppResult<Config> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| AppError::Config(format!("{}: {e}", path.display())))?;

        // Older files name services where a kind belongs. Rewriting them in
        // passing is not a nicety: the program changed the format, so the
        // program carries the cost — refusing to start over a retired word 
        // retired would be charging it to the user.
        let mut value: toml::Value = toml::from_str(&text)
            .map_err(|e| AppError::Config(format!("{}: {e}", path.display())))?;
        let migrated = migrate(&mut value);

        let cfg: Config =
            value.try_into().map_err(|e| AppError::Config(format!("{}: {e}", path.display())))?;
        cfg.validate()?;

        if migrated {
            tracing::info!("migrated {} to the current format", path.display());
            cfg.save(path)?;
        }
        Ok(cfg)
    }

    /// Load from `path`, writing the starter config first if nothing is there.
    ///
    /// An empty source list is the wall a new user hits, so a fresh install
    /// arrives with working feeds rather than an invitation to go find some.
    pub fn load_or_init(path: &Path) -> AppResult<Config> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, STARTER_CONFIG)?;
            tracing::info!("wrote starter configuration to {}", path.display());
        }
        Config::load(path)
    }

    /// Write the configuration back out.
    ///
    /// Serializing replaces the whole file, so comments in it are lost. A
    /// header is written back to say that the application edits this file.
    pub fn save(&self, path: &Path) -> AppResult<()> {
        self.validate()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| AppError::Config(format!("could not serialise the configuration: {e}")))?;

        // Write beside the target and rename: a half-written config file is a
        // program that will not start.
        let tmp = path.with_extension("toml.new");
        std::fs::write(&tmp, format!("{CONFIG_HEADER}\n{body}"))?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// The window a collection run should use.
    ///
    /// An explicit date wins over the rolling window, because someone who typed
    /// a date meant it.
    pub fn date_range(&self) -> crate::news::filters::DateRange {
        use crate::news::filters::DateRange;
        use chrono::{NaiveDate, NaiveTime, TimeZone, Utc};

        let parse = |value: &Option<String>, end_of_day: bool| {
            value.as_deref().filter(|v| !v.trim().is_empty()).and_then(|v| {
                NaiveDate::parse_from_str(v.trim(), "%Y-%m-%d").ok().map(|d| {
                    let time = if end_of_day {
                        NaiveTime::from_hms_opt(23, 59, 59).unwrap()
                    } else {
                        NaiveTime::MIN
                    };
                    Utc.from_utc_datetime(&d.and_time(time))
                })
            })
        };

        let since = parse(&self.settings.collect_from, false);
        let until = parse(&self.settings.collect_to, true);

        if since.is_some() || until.is_some() {
            return DateRange { since, until };
        }
        match self.settings.collect_days {
            0 => DateRange::default(),
            days => DateRange::last_days(days),
        }
    }

    /// The moment before which stored articles may be deleted, if any.
    ///
    /// Never inside the collection window: the next run would fetch the same
    /// articles again.
    pub fn prune_before(&self) -> Option<chrono::DateTime<chrono::Utc>> {
        if self.settings.keep_days <= 0 {
            return None;
        }
        let mut cutoff = chrono::Utc::now() - chrono::Duration::days(self.settings.keep_days);
        if let Some(since) = self.date_range().since {
            cutoff = cutoff.min(since);
        }
        Some(cutoff)
    }

    pub fn add_source(&mut self, spec: SourceSpec) {
        self.sources.push(spec);
    }

    /// Put `spec` in place of the source at `index`, keeping the fields the
    /// window's form cannot describe: the discussion template and
    /// `url_from_discussion`, which the shipped Hacker News entry uses.
    pub fn replace_source(&mut self, index: usize, mut spec: SourceSpec) -> bool {
        let Some(old) = self.sources.get(index) else {
            return false;
        };

        spec.enabled = old.enabled;
        if let (Some(map), Some(kept)) = (spec.map.as_mut(), old.map.as_ref()) {
            map.comments = kept.comments.clone();
            map.discussion = kept.discussion.clone();
            map.discussion_template = kept.discussion_template.clone();
            map.url_from_discussion = kept.url_from_discussion;
            map.excerpt = kept.excerpt.clone();
        }
        if let (Some(selectors), Some(kept)) = (spec.selectors.as_mut(), old.selectors.as_ref()) {
            selectors.strip = kept.strip.clone();
        }

        self.sources[index] = spec;
        true
    }

    /// Remove the source at `index`, returning whether there was one.
    pub fn remove_source(&mut self, index: usize) -> bool {
        if index >= self.sources.len() {
            return false;
        }
        self.sources.remove(index);
        true
    }

    pub fn set_enabled(&mut self, index: usize, enabled: bool) -> bool {
        match self.sources.get_mut(index) {
            Some(s) => {
                s.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Refusals a person can act on, so each is named rather than written: the
    /// same complaint reaches a config file being edited by hand and a form
    /// being filled in in one of seven languages.
    fn validate(&self) -> AppResult<()> {
        for (i, s) in self.sources.iter().enumerate() {
            let what = s.title.clone().unwrap_or_else(|| format!("source #{}", i + 1));
            match s.kind {
                Kind::Rss | Kind::HtmlList | Kind::JsonApi if s.url.is_none() => {
                    return Err(AppError::told_about(
                        "source_needs_url",
                        format!("{what}: '{:?}' needs a url", s.kind),
                        what,
                    ));
                }
                Kind::HtmlList if s.selectors.is_none() => {
                    return Err(AppError::told_about(
                        "source_needs_selectors",
                        format!("{what}: html_list needs [selectors]"),
                        what,
                    ));
                }
                Kind::JsonApi if s.map.is_none() => {
                    return Err(AppError::told_about(
                        "source_needs_map",
                        format!("{what}: json_api needs [source.map] saying where the link lives"),
                        what,
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The service the settings are pointing at, if it is still in the list.
    pub fn chosen_service(&self) -> Option<&Service> {
        // The address and model outlive the choice — switching to a local model
        // leaves them in place so switching back is not a retyping — so the
        // provider is what says whether they are in use. Without this both
        // lists would mark something as chosen while only one of them is.
        if self.settings.provider != Provider::OpenAi {
            return None;
        }
        let (url, model) = (self.settings.api_url.as_deref()?, self.settings.api_model.as_deref()?);
        self.services.iter().find(|s| s.is(url, model))
    }

    /// Add a service, or replace the one that already has this address and
    /// model. Saving the same pair twice is a correction, not a second entry.
    pub fn save_service(&mut self, service: Service) {
        match self.services.iter_mut().find(|s| s.is(&service.url, &service.model)) {
            Some(existing) => *existing = service,
            None => self.services.push(service),
        }
    }

    /// Remove the service at `index`, returning whether there was one. If it
    /// was the one in use, the settings stop pointing at it: a chosen service
    /// that is not in the list would be a summary written by nothing.
    pub fn remove_service(&mut self, index: usize) -> bool {
        if index >= self.services.len() {
            return false;
        }
        let gone = self.services.remove(index);
        if self.settings.api_url.as_deref() == Some(gone.url.as_str())
            && self.settings.api_model.as_deref() == Some(gone.model.as_str())
        {
            self.settings.api_url = None;
            self.settings.api_model = None;
            self.settings.provider = Provider::Local;
        }
        true
    }

    pub fn enabled_sources(&self) -> impl Iterator<Item = &SourceSpec> {
        self.sources.iter().filter(|s| s.enabled)
    }
}

impl SourceSpec {
    /// Human-readable name for logs and the UI.
    pub fn label(&self) -> String {
        if let Some(t) = &self.title {
            return t.clone();
        }
        self.url.clone().unwrap_or_else(|| "unnamed source".into())
    }

    /// The address to put a question to, when there is one.
    ///
    /// An empty string collects the same as no address, but stays in the file
    /// so it is not filled in again on the next start.
    pub fn search_template(&self) -> Option<&str> {
        self.search_url.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// The map's address, when one is written down.
    pub fn sitemap_template(&self) -> Option<&str> {
        self.sitemap_url.as_deref().map(str::trim).filter(|s| !s.is_empty())
    }

    /// Whether to look this publisher's map up.
    ///
    /// An empty address means the reader established there is none; the empty
    /// string in the file is what stops the check on every run.
    pub fn maps_wanted(&self) -> bool {
        self.sitemap_url.as_deref().map(str::trim) != Some("")
    }
}

/// Move a service out of `[settings]` into the list of services.
fn the_one_service_becomes_the_first_of_many(value: &mut toml::Value) -> bool {
    let Some(root) = value.as_table_mut() else {
        return false;
    };
    if root.contains_key("service") {
        return false;
    }
    let Some(settings) = root.get_mut("settings").and_then(|s| s.as_table_mut()) else {
        return false;
    };
    // The key is what moves; the address and model stay behind as the pointer
    // to which service is in use.
    let key = settings.remove("api_key");
    let (Some(url), Some(model)) = (settings.get("api_url"), settings.get("api_model")) else {
        return key.is_some();
    };

    let mut service = toml::map::Map::new();
    service.insert("url".into(), url.clone());
    service.insert("model".into(), model.clone());
    if let Some(key) = key.filter(|k| k.as_str().is_some_and(|k| !k.trim().is_empty())) {
        service.insert("key".into(), key);
    }
    root.insert("service".into(), toml::Value::Array(vec![toml::Value::Table(service)]));
    true
}

/// Drop the `[search]` block; nothing reads it since sitemaps replaced it.
fn the_search_engine_is_gone(value: &mut toml::Value) -> bool {
    value.as_table_mut().is_some_and(|root| root.remove("search").is_some())
}

/// Write Russian into a configuration that predates the language setting.
///
/// English is the default for a new install; an existing file was written when
/// the interface spoke only Russian, so its absence means Russian was chosen.
fn keep_the_language_the_reader_already_had(value: &mut toml::Value) -> bool {
    let Some(root) = value.as_table_mut() else {
        return false;
    };
    let settings = root
        .entry("settings")
        .or_insert_with(|| toml::Value::Table(toml::map::Map::new()))
        .as_table_mut();
    let Some(settings) = settings else {
        return false;
    };
    if settings.contains_key("ui_lang") {
        return false;
    }
    settings.insert("ui_lang".into(), UiLang::Ru.tag().into());
    // The summary language moved its default at the same time, and for the same
    // reason. A file that never named one was written when the answer could
    // only be Russian.
    settings.entry("output_lang").or_insert_with(|| Lang::Ru.code().into());
    true
}

/// Rewrite entries written against a retired format.
///
/// Returns whether anything changed, so the caller saves the result and the
/// migration runs once rather than on every start.
fn migrate(value: &mut toml::Value) -> bool {
    let mut changed = keep_the_language_the_reader_already_had(value);
    changed |= the_one_service_becomes_the_first_of_many(value);
    changed |= the_search_engine_is_gone(value);
    let mut rescued: Vec<String> = Vec::new();

    // `changed`, not `false`: the two migrations above have already run, and
    // discarding their answer here left a configuration with no sources being
    // migrated again on every start because it was never saved.
    let Some(sources) = value.get_mut("source").and_then(|s| s.as_array_mut()) else {
        return changed;
    };

    for source in sources.iter_mut() {
        let Some(table) = source.as_table_mut() else {
            continue;
        };
        let enabled = table.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
        if let Some(query) = table.remove("query") {
            changed = true;
            // Only from a source that was actually being collected. A query
            // sitting in a switched-off preset is not something the reader
            // asked for, and promoting it would silently narrow every other
            // source to a subject they never chose.
            match query.as_str() {
                Some(text) if enabled => rescued.extend(
                    text.split(" OR ").map(|t| t.trim().to_string()).filter(|t| !t.is_empty()),
                ),
                _ => {}
            }
        }
    }

    if !rescued.is_empty() {
        let settings = value
            .as_table_mut()
            .expect("a config file is a table")
            .entry("settings")
            .or_insert_with(|| toml::Value::Table(toml::map::Map::new()));
        if let Some(table) = settings.as_table_mut() {
            let existing =
                table.get("keywords").and_then(|k| k.as_array()).map(|a| a.len()).unwrap_or(0);
            // Only when the reader has not said what they want. Their own list
            // is a decision; a query left in a preset is an accident of format.
            if existing == 0 {
                rescued.sort();
                rescued.dedup();
                tracing::info!("moved source queries into the keywords: {rescued:?}");
                table.insert(
                    "keywords".into(),
                    toml::Value::Array(rescued.into_iter().map(toml::Value::from).collect()),
                );
            }
        }
    }

    let sources = value.get_mut("source").and_then(|s| s.as_array_mut()).expect("checked above");

    for source in sources {
        let Some(table) = source.as_table_mut() else {
            continue;
        };
        let kind = table.get("kind").and_then(|k| k.as_str()).unwrap_or_default().to_string();

        match kind.as_str() {
            "hacker_news" => {
                table.insert("kind".into(), "json_api".into());
                table.entry("url").or_insert_with(|| HN_SEARCH_URL.into());
                table.entry("lang").or_insert_with(|| "en".into());
                table.entry("map").or_insert_with(|| {
                    let mut map = toml::map::Map::new();
                    for (key, path) in [
                        ("items", "hits"),
                        ("url", "url"),
                        ("title", "title"),
                        ("published", "created_at_i"),
                        ("score", "points"),
                        ("comments", "num_comments"),
                        ("excerpt", "story_text"),
                        ("discussion_template", HN_ITEM_URL),
                    ] {
                        map.insert(key.into(), path.into());
                    }
                    map.insert("url_from_discussion".into(), true.into());
                    toml::Value::Table(map)
                });
                changed = true;
            }
            "google_news" => {
                let lang = table.get("lang").and_then(|l| l.as_str()).unwrap_or("en").to_string();
                table.insert("kind".into(), "rss".into());
                table.entry("url").or_insert_with(|| google_news_url(&lang).into());
                changed = true;
            }
            _ => {}
        }
    }
    changed
}

const HN_SEARCH_URL: &str = "https://hn.algolia.com/api/v1/search_by_date\
?tags=story&hitsPerPage=50&query={query}\
&numericFilters=points%3E{min_points}%2Ccreated_at_i%3E{since}";

const HN_ITEM_URL: &str = "https://news.ycombinator.com/item?id={objectID}";

/// Google's regional parameters. All three must agree, or the results come
/// back in another language.
fn google_region(lang: &str) -> (&'static str, &'static str) {
    match lang {
        "ru" => ("RU", "RU:ru"),
        "de" => ("DE", "DE:de"),
        "fr" => ("FR", "FR:fr"),
        "es" => ("ES", "ES:es"),
        "pt" => ("BR", "BR:pt-419"),
        _ => ("US", "US:en"),
    }
}

fn google_news_url(lang: &str) -> String {
    let (gl, ceid) = google_region(lang);
    format!("https://news.google.com/rss/search?q={{query}}&hl={lang}&gl={gl}&ceid={ceid}")
}

/// Feeds served from a host that is not the one the articles live on.
///
/// Stripping a `feeds.`/`www.` label covers every other shipped publisher; the
/// BBC serves feeds from a domain that hosts no articles and no sitemap.
const FEED_HOST_IS_NOT_THE_SITE: &[(&str, &str)] = &[("feeds.bbci.co.uk", "bbc.co.uk")];

/// The site an address publishes under, which is where its map will be.
pub fn publisher_site(url: &str) -> Option<String> {
    let host = url::Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
    if let Some((_, site)) = FEED_HOST_IS_NOT_THE_SITE.iter().find(|(feed, _)| *feed == host) {
        return Some((*site).to_string());
    }
    let site = ["www.", "feeds.", "feed.", "rss."]
        .iter()
        .find_map(|prefix| host.strip_prefix(prefix))
        .unwrap_or(host.as_str());
    (!site.is_empty()).then(|| site.to_string())
}

const STARTER_CONFIG: &str = include_str!("../resources/sources.default.toml");

const CONFIG_HEADER: &str = "\
# Shadow Tentacles — sources and settings.
#
# Edit this by hand or from the application's Sources screen; both write here.
# Note that the application rewrites the whole file when it saves, so comments
# added below this header do not survive a change made in the window.
#
# kind = \"rss\"        a feed — RSS, Atom or JSON Feed
# kind = \"json_api\"   any service answering with JSON; needs [source.map]
# kind = \"html_list\"  a listing page for a site with no feed; needs [source.selectors]
#
# A source has two addresses. `url` is its own feed, and it is read every time.
# `search_url` is for the few sources that can be asked a question — it takes
# {query}. Most publishers cannot: a feed hands out its own tail and answers
# nothing. For those, configure a search engine under [search] and it will be
# asked about that site instead; the feed is still read either way, so a keyword
# never means fewer articles than no keyword.
#
# An address may carry {query}, {days}, {min_points}, {since}, {until},
# {since_date} and {until_date}. They are filled from the settings before the
# request, so the service does the filtering instead of this program discarding
# the results afterwards.
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_old_hacker_news_entry_still_works() {
        let old = r#"
            [[source]]
            kind = "hacker_news"
            title = "Hacker News"
            min_points = 100
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value), "the entry should have been rewritten");

        let cfg: Config = value.try_into().unwrap();
        cfg.validate().unwrap();

        let source = &cfg.sources[0];
        assert_eq!(source.kind, Kind::JsonApi);
        assert_eq!(source.min_points, Some(100), "the user's own setting must survive");
        let map = source.map.as_ref().expect("a field map was filled in");
        assert_eq!(map.items, "hits");
        assert!(map.url_from_discussion, "Ask HN posts must keep working");
    }

    #[test]
    fn an_old_google_news_entry_still_works() {
        let old = r#"
            [[source]]
            kind = "google_news"
            query = "тест"
            lang = "ru"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value));

        let cfg: Config = value.try_into().unwrap();
        let source = &cfg.sources[0];
        assert_eq!(source.kind, Kind::Rss);
        let url = source.url.as_deref().unwrap();
        assert!(url.contains("hl=ru") && url.contains("gl=RU"), "{url}");
        assert!(url.contains("{query}"), "the query must still reach the address: {url}");
    }

    #[test]
    fn a_query_written_into_a_source_becomes_a_keyword() {
        let old = r#"
            [[source]]
            kind = "rss"
            url = "https://news.google.com/rss/search?q={query}"
            query = "локальные нейросети OR local LLM"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value));

        let cfg: Config = value.try_into().unwrap();
        assert_eq!(cfg.settings.keywords, vec!["local LLM", "локальные нейросети"]);
        assert_eq!(
            cfg.settings.search_terms(),
            vec!["local LLM", "локальные нейросети"],
            "и уйти обратно в адрес источника они должны по одному"
        );
    }

    #[test]
    fn a_switched_off_preset_does_not_impose_its_query() {
        let old = r#"
            [[source]]
            kind = "rss"
            url = "https://news.google.com/rss/search?q={query}"
            query = "локальные нейросети"
            enabled = false
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value), "поле всё равно должно уйти");

        let cfg: Config = value.try_into().unwrap();
        assert!(
            cfg.settings.keywords.is_empty(),
            "фильтр по словам, которых читатель не выбирал, сузил бы всю ленту"
        );
    }

    #[test]
    fn a_readers_own_keywords_win_over_a_rescued_query() {
        let old = r#"
            [settings]
            output_lang = "ru"
            keywords = ["санкции"]

            [[source]]
            kind = "rss"
            url = "https://example.com/rss?q={query}"
            query = "что-то из пресета"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value));

        let cfg: Config = value.try_into().unwrap();
        assert_eq!(cfg.settings.keywords, vec!["санкции"], "решение читателя не переписывают");
    }

    #[test]
    fn exclusions_stay_at_home() {
        let settings = Settings {
            keywords: vec!["санкции".into(), "-спорт".into()],
            ..Settings::default()
        };
        // No service has a dependable exclusion syntax, so a minus stays
        // ours to apply against the text.
        assert_eq!(settings.search_terms(), vec!["санкции"]);
    }

    #[test]
    fn a_feed_host_is_not_always_the_site() {
        assert_eq!(publisher_site("https://meduza.io/rss/all").as_deref(), Some("meduza.io"));
        assert_eq!(
            publisher_site("https://www.lemonde.fr/rss/une.xml").as_deref(),
            Some("lemonde.fr")
        );
        // The one publisher whose feeds live somewhere no article ever does.
        assert_eq!(
            publisher_site("https://feeds.bbci.co.uk/news/world/rss.xml").as_deref(),
            Some("bbc.co.uk")
        );
        assert_eq!(publisher_site("not an address"), None);
    }

    #[test]
    fn a_current_config_is_left_alone() {
        let mut value: toml::Value = toml::from_str(STARTER_CONFIG).unwrap();
        assert!(!migrate(&mut value), "nothing to migrate in a file already current");
    }

    #[test]
    fn a_hand_written_url_is_not_overwritten() {
        let old = r#"
            [[source]]
            kind = "hacker_news"
            url = "https://hn.algolia.com/api/v1/search?tags=front_page"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        migrate(&mut value);
        let cfg: Config = value.try_into().unwrap();
        assert_eq!(
            cfg.sources[0].url.as_deref(),
            Some("https://hn.algolia.com/api/v1/search?tags=front_page"),
            "a user who edited the address meant it"
        );
    }

    #[test]
    fn a_fresh_install_speaks_english() {
        let cfg: Config = toml::from_str(STARTER_CONFIG).unwrap();
        assert_eq!(cfg.settings.ui_lang, UiLang::En);
        assert_eq!(cfg.settings.output_lang, Lang::En, "окно и выжимки не должны спорить");
        assert_eq!(Settings::default().ui_lang, UiLang::En);
    }

    #[test]
    fn a_configuration_written_before_the_choice_keeps_russian() {
        // The program spoke nothing else when this file was written, so its
        // owner has been reading Russian; English is the default for people who
        // have not chosen, not a correction of the ones who have.
        let old = r#"
            [settings]
            collect_days = 3

            [[source]]
            kind = "rss"
            url = "https://meduza.io/rss/all"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value));

        let cfg: Config = value.try_into().unwrap();
        assert_eq!(cfg.settings.ui_lang, UiLang::Ru);
        assert_eq!(cfg.settings.output_lang, Lang::Ru);
    }

    #[test]
    fn a_configuration_that_names_a_language_is_left_alone() {
        let chosen = r#"
            [settings]
            ui_lang = "fr"
            output_lang = "en"
        "#;
        let mut value: toml::Value = toml::from_str(chosen).unwrap();
        assert!(!migrate(&mut value), "нечего мигрировать — выбор уже сделан");

        let cfg: Config = value.try_into().unwrap();
        assert_eq!(cfg.settings.ui_lang, UiLang::Fr);
        assert_eq!(cfg.settings.output_lang, Lang::En);
    }

    #[test]
    fn a_configuration_that_still_names_an_engine_loses_it() {
        let old = r#"
            [settings]
            ui_lang = "en"

            [search]
            preset = "gdelt"
            url = "https://api.gdeltproject.org/api/v2/doc/doc?query={query}"

            [search.map]
            items = "articles"
            url = "url"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value));
        assert!(value.get("search").is_none(), "нечего оставлять — движка больше нет");
        // And it still loads, which is the point of migrating rather than
        // refusing to start over a retired word  retired.
        let cfg: Config = value.try_into().unwrap();
        assert_eq!(cfg.settings.ui_lang, UiLang::En);
    }

    #[test]
    fn a_publishers_map_is_looked_up_unless_the_reader_says_otherwise() {
        let mut spec = SourceSpec {
            kind: Kind::Rss,
            title: Some("T".into()),
            url: Some("https://example.com/rss".into()),
            search_url: None,
            sitemap_url: None,
            lang: None,
            enabled: true,
            min_points: None,
            follow_external: true,
            selectors: None,
            map: None,
            headers: Default::default(),
        };
        // Nothing written down: go and look, every run.
        assert!(spec.maps_wanted());
        assert_eq!(spec.sitemap_template(), None);

        // An address written down is used instead of looking.
        spec.sitemap_url = Some("https://example.com/sitemap-news.xml".into());
        assert_eq!(spec.sitemap_template(), Some("https://example.com/sitemap-news.xml"));

        // An emptied address is a reader who established there is no map. It
        // has to survive as an empty string, or the check runs again tomorrow.
        spec.sitemap_url = Some(String::new());
        assert!(!spec.maps_wanted());
        assert_eq!(spec.sitemap_template(), None);
    }

    #[test]
    fn the_provider_is_written_the_way_a_person_types_it() {
        let settings = Settings { provider: Provider::OpenAi, ..Settings::default() };
        let written = toml::to_string(&settings).unwrap();
        assert!(written.contains("provider = \"openai\""), "{written}");

        let back: Settings = toml::from_str(&written).unwrap();
        assert_eq!(back.provider, Provider::OpenAi);
        assert_eq!("openai".parse::<Provider>(), Ok(Provider::OpenAi));
        assert_eq!("local".parse::<Provider>(), Ok(Provider::Local));
    }

    #[test]
    fn every_interface_language_survives_a_round_trip() {
        // The tag is what the file holds and what the window asks its dictionary
        // for; a mismatch would be a screen that silently falls back to English.
        for lang in UiLang::ALL {
            assert_eq!(lang.tag().parse::<UiLang>(), Ok(lang));
            let written = toml::to_string(&Settings { ui_lang: lang, ..Settings::default() })
                .expect("settings serialise");
            assert!(written.contains(&format!("ui_lang = \"{}\"", lang.tag())), "{written}");
        }
    }

    #[test]
    fn editing_a_source_keeps_what_the_form_cannot_say() {
        // Hacker News as it ships: the discussion link is built from the item's
        // id, and posts with no outbound link point at their own thread. The
        // window's form has no field for either, so editing the score threshold
        // must not quietly turn this into a different source.
        let mut cfg: Config = toml::from_str(STARTER_CONFIG).unwrap();
        let at = cfg
            .sources
            .iter()
            .position(|s| s.title.as_deref() == Some("Hacker News"))
            .expect("the starter configuration ships it");
        cfg.sources[at].enabled = false;

        let mut edited = cfg.sources[at].clone();
        edited.min_points = Some(250);
        edited.enabled = true; // the form does not send this either
        edited.map.as_mut().unwrap().discussion_template = None;
        edited.map.as_mut().unwrap().url_from_discussion = false;

        assert!(cfg.replace_source(at, edited));

        let now = &cfg.sources[at];
        assert_eq!(now.min_points, Some(250), "что форма сказала — то и записано");
        assert!(!now.enabled, "переключатель живёт отдельно от формы");
        let map = now.map.as_ref().unwrap();
        assert!(map.url_from_discussion, "Ask HN должны продолжать работать");
        assert!(map.discussion_template.is_some());
    }

    #[test]
    fn replacing_a_source_that_is_not_there_changes_nothing() {
        let mut cfg: Config = toml::from_str(STARTER_CONFIG).unwrap();
        let before = cfg.sources.len();
        let spec = cfg.sources[0].clone();
        assert!(!cfg.replace_source(before + 3, spec));
        assert_eq!(cfg.sources.len(), before);
    }

    #[test]
    fn the_one_service_moves_into_the_list_and_stays_chosen() {
        let old = r#"
            [settings]
            ui_lang = "ru"
            provider = "openai"
            api_url = "https://openrouter.ai/api/v1"
            api_key = "sk-or-secret"
            api_model = "minimax/minimax-m3:free"
        "#;
        let mut value: toml::Value = toml::from_str(old).unwrap();
        assert!(migrate(&mut value));

        let cfg: Config = value.try_into().unwrap();
        assert_eq!(cfg.services.len(), 1, "то, что было настроено, не теряется");
        assert_eq!(cfg.services[0].key.as_deref(), Some("sk-or-secret"));
        let chosen = cfg.chosen_service().expect("выбор остаётся за ним");
        assert_eq!(chosen.model, "minimax/minimax-m3:free");

        // The key moved: it lives with the service now, and nowhere else.
        let written = toml::to_string(&cfg).unwrap();
        assert!(!written.contains("api_key"), "{written}");
    }

    #[test]
    fn saving_the_same_address_and_model_corrects_it_rather_than_doubling_it() {
        let mut cfg = Config::default();
        let service = |key: &str| Service {
            url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            key: Some(key.into()),
        };
        cfg.save_service(service("first"));
        cfg.save_service(service("second"));

        assert_eq!(cfg.services.len(), 1, "это исправление, а не вторая запись");
        assert_eq!(cfg.services[0].key.as_deref(), Some("second"));
    }

    #[test]
    fn only_one_of_the_two_lists_holds_the_choice() {
        let mut cfg = Config::default();
        cfg.save_service(Service {
            url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            key: None,
        });
        cfg.settings.api_url = Some("https://api.openai.com/v1".into());
        cfg.settings.api_model = Some("gpt-4o-mini".into());

        cfg.settings.provider = Provider::OpenAi;
        assert!(cfg.chosen_service().is_some());

        // Choosing a local model leaves the address behind so that coming back
        // is not a retyping — but nothing external is writing the summaries any
        // more, and the list must not say otherwise.
        cfg.settings.provider = Provider::Local;
        assert!(cfg.chosen_service().is_none());
        assert!(cfg.settings.api_url.is_some(), "адрес остаётся, чтобы вернуться без набора");
    }

    #[test]
    fn removing_the_service_in_use_stops_it_being_in_use() {
        let mut cfg = Config::default();
        cfg.save_service(Service {
            url: "https://api.openai.com/v1".into(),
            model: "gpt-4o-mini".into(),
            key: None,
        });
        cfg.settings.provider = Provider::OpenAi;
        cfg.settings.api_url = Some("https://api.openai.com/v1".into());
        cfg.settings.api_model = Some("gpt-4o-mini".into());
        assert!(cfg.chosen_service().is_some());

        assert!(cfg.remove_service(0));
        assert!(cfg.chosen_service().is_none());
        assert_eq!(
            cfg.settings.provider,
            Provider::Local,
            "выжимки должен кто-то писать, и это снова эта машина"
        );
        assert!(!cfg.remove_service(0), "второй раз удалять нечего");
    }

    #[test]
    fn starter_config_is_valid() {
        let cfg: Config = toml::from_str(STARTER_CONFIG).expect("starter config must parse");
        cfg.validate().expect("starter config must validate");
        assert!(!cfg.sources.is_empty(), "starter config must ship real sources");
    }

    #[test]
    fn rss_without_url_is_rejected() {
        let cfg: Config = toml::from_str("[[source]]\nkind = \"rss\"\n").unwrap();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn an_explicit_window_beats_the_rolling_one() {
        let mut cfg = Config::default();
        cfg.settings.collect_days = 3;
        cfg.settings.collect_from = Some("2026-08-01".into());
        cfg.settings.collect_to = Some("2026-08-07".into());

        let range = cfg.date_range();
        let since = range.since.expect("a start was given");
        let until = range.until.expect("an end was given");
        assert_eq!(since.format("%Y-%m-%d").to_string(), "2026-08-01");
        // The closing day counts in full, or a one-day window would select nothing.
        assert_eq!(until.format("%Y-%m-%d %H:%M").to_string(), "2026-08-07 23:59");
    }

    #[test]
    fn a_blank_date_falls_back_to_the_rolling_window() {
        let mut cfg = Config::default();
        cfg.settings.collect_days = 3;
        cfg.settings.collect_from = Some("   ".into());
        assert!(cfg.date_range().since.is_some());
        assert!(cfg.date_range().until.is_none());
    }

    #[test]
    fn zero_days_and_no_dates_means_everything() {
        let mut cfg = Config::default();
        cfg.settings.collect_days = 0;
        assert!(cfg.date_range().is_open());
    }

    #[test]
    fn a_saved_config_reads_back_identically() {
        let dir = std::env::temp_dir().join(format!("shadow-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sources.toml");

        let mut cfg: Config = toml::from_str(STARTER_CONFIG).unwrap();
        cfg.settings.keywords = vec!["санкции".into(), "-спорт".into()];
        cfg.settings.collect_days = 7;
        // A reader who emptied a search address on purpose. It has to come back
        // as an empty string, not as nothing: nothing would be filled in again
        // on the next start, which is the decision being overruled.
        cfg.sources[0].search_url = Some(String::new());
        cfg.save(&path).unwrap();

        let back = Config::load(&path).unwrap();
        assert_eq!(back.settings.keywords, cfg.settings.keywords);
        assert_eq!(back.settings.collect_days, 7);
        assert_eq!(back.sources.len(), cfg.sources.len());
        assert_eq!(back.sources[0].search_url.as_deref(), Some(""));
        assert_eq!(back.sources[1].search_url, cfg.sources[1].search_url);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn an_invalid_config_is_never_written() {
        // Saving must fail before it can leave something unloadable on disk.
        let dir = std::env::temp_dir().join(format!("shadow-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sources.toml");

        let mut cfg = Config::default();
        cfg.add_source(SourceSpec {
            kind: Kind::Rss,
            title: Some("без адреса".into()),
            url: None,
            search_url: None,
            sitemap_url: None,
            lang: None,
            enabled: true,
            min_points: None,
            follow_external: true,
            selectors: None,
            map: None,
            headers: Default::default(),
        });
        assert!(cfg.save(&path).is_err());
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn disabled_sources_are_skipped() {
        let cfg: Config = toml::from_str(
            "[[source]]\nkind = \"rss\"\nurl = \"https://a/feed\"\nenabled = false\n\
             [[source]]\nkind = \"rss\"\nurl = \"https://b/feed\"\n",
        )
        .unwrap();
        assert_eq!(cfg.enabled_sources().count(), 1);
    }
}
