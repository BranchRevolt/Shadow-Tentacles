// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! The command-line front end, over the same core the window calls.

use std::path::PathBuf;
use std::time::Instant;

use crate::config::{Config, Provider};
use crate::error::{AppError, AppResult};
use crate::llm::{Engine, Lang, Mode, prompt_version};
use crate::models::{Hardware, downloader, registry};
use crate::news::Fetcher;
use crate::news::filters::DateRange;
use crate::paths;
use crate::pipeline;
use crate::shutdown::Cancel;
use crate::store::{Selection, Store};

pub fn run(args: &[String]) -> AppResult<()> {
    match args.first().map(String::as_str) {
        Some("doctor") => doctor(),
        Some("sources") => sources(),
        Some("fetch") => block_on(fetch(&args[1..])),
        Some("summarize") => summarize(&args[1..]),
        Some("show") => show(&args[1..]),
        Some("recheck") => recheck(&args[1..]),
        Some("prune") => prune(&args[1..]),
        Some("speak") => speak(&args[1..]),
        Some("model") => block_on(model(&args[1..])),
        Some("run") => {
            block_on(fetch(&args[1..]))?;
            summarize(&args[1..])?;
            show(&args[1..])
        }
        Some("help") | Some("--help") | Some("-h") | None => {
            print_usage();
            Ok(())
        }
        Some(other) => {
            Err(AppError::Other(format!("unknown command '{other}' — run `shadow-tentacles help`")))
        }
    }
}

/// Run an async command on a single-threaded runtime.
///
/// One thread is enough: the concurrency that matters is I/O, which the runtime
/// multiplexes anyway, and a current-thread runtime keeps the non-`Sync` SQLite
/// connection usable throughout without wrapping it in a mutex.
fn block_on<F: std::future::Future<Output = AppResult<()>>>(f: F) -> AppResult<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| AppError::Other(e.to_string()))?
        .block_on(f)
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn doctor() -> AppResult<()> {
    let hw = Hardware::probe();
    println!("hardware:   {}", hw.describe());

    let suggested = registry::recommend(hw.vram_mb, hw.ram_mb);
    println!("suggested:  {} [{}]", suggested.name, suggested.tier.label());
    println!("build:      {}", if hw.vram_mb > 0 { "GPU" } else { "CPU (no GPU detected)" });
    println!("prompts:    {} / {}", prompt_version(Mode::Direct), prompt_version(Mode::TwoPass));
    println!("config:     {}", paths::sources_path()?.display());
    println!("database:   {}", paths::database_path()?.display());
    // The reader's own choice if they made one, so this line answers "where
    // will the next model land" rather than "where would it land by default".
    let config = Config::load_or_init(&paths::sources_path()?).ok();
    let chosen = config.as_ref().and_then(|c| c.settings.models_dir.clone());
    println!("models:     {}", paths::models_dir_or(chosen.as_deref())?.display());
    match config.as_ref().map(|c| &c.settings) {
        Some(set) if set.provider == Provider::OpenAi => {
            println!(
                "summaries:  {} at {} — article text is sent there",
                set.api_model.as_deref().unwrap_or("(no model named)"),
                set.api_url.as_deref().unwrap_or("(no address)")
            );
        }
        _ => println!("summaries:  written on this machine"),
    }

    let db = paths::database_path()?;
    if db.exists() {
        let store = Store::open(&db)?;
        // The language decides what counts as written up, so a store with no
        // configuration to read is reported against the default.
        let lang = config.as_ref().map(|c| c.settings.output_lang).unwrap_or(Lang::En);
        let version = crate::llm::prompt_version(crate::llm::Mode::TwoPass);
        let model = config.as_ref().and_then(|c| model_id(c).ok());
        let all = Selection::everything();
        println!(
            "stored:     {} article(s), {} written up, {} waiting",
            store.count_articles(&all)?,
            store.count_summaries(lang, &version, &all)?,
            store.count_pending(lang, &version, model.as_deref(), &all)?,
        );
        println!();
        // Diagnostics report the whole store, not the current selection: the
        // question here is what is on disk.
        for (status, reason, count) in store.verdict_breakdown(&Selection::everything())? {
            println!("  {count:>5}  {status:<14} {}", reason.unwrap_or_default());
        }
    }
    Ok(())
}

fn sources() -> AppResult<()> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    println!("output language: {}", config.settings.output_lang);
    if !config.settings.keywords.is_empty() {
        println!("keywords:        {}", config.settings.keywords.join(", "));
    }
    println!();
    for spec in &config.sources {
        println!(
            "  {} {:<40} {:?}{}",
            if spec.enabled { "▸" } else { "·" },
            spec.label(),
            spec.kind,
            spec.lang.map(|l| format!(" [{l}]")).unwrap_or_default()
        );
    }
    Ok(())
}

async fn fetch(args: &[String]) -> AppResult<()> {
    let opts = Options::parse(args)?;
    let config = load_config(&opts)?;
    let store = Store::open(&paths::database_path()?)?;
    let fetcher = Fetcher::new()?;
    let cancel = Cancel::new();

    // `--days` overrides for one run; without it the window and the command
    // line collect over exactly the same period.
    let range = match opts.days {
        Some(days) => {
            println!("collecting articles from the last {days} day(s)…");
            DateRange::last_days(days)
        }
        None => {
            let range = config.date_range();
            match (range.since, range.until) {
                (None, None) => println!("collecting everything the sources offer…"),
                (from, to) => println!(
                    "collecting articles published {} … {}",
                    from.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "—".into()),
                    to.map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_else(|| "now".into()),
                ),
            }
            range
        }
    };

    let started = Instant::now();
    // One line, rewritten in place: a terminal that scrolls a line per page is
    // worse than no progress at all.
    let report = pipeline::collect(&config, &store, &fetcher, range, &cancel, |stage, n, of| {
        let what = match stage {
            pipeline::Stage::Polling => "polling",
            pipeline::Stage::Reading => "reading",
        };
        print!("\r{what} {n}/{of}   ");
        let _ = std::io::Write::flush(&mut std::io::stdout());
    })
    .await?;
    println!();
    store.checkpoint()?;

    for (name, dropped) in &report.sources_cut {
        println!("{name}: {dropped} more match(es) than one run will read, newest kept");
    }
    let names = |what: &[String]| match what {
        [] => String::new(),
        some => format!(" ({})", some.join(", ")),
    };
    println!(
        "\n{} source(s) polled, {} answered from their map, {} by address only{}, \
         {} from the feed alone{}, {} with no answer\n\
         {} candidate(s): {} known, \
         {} out of range, {} unreachable, {} rejected, {} off topic\n\
         {} stored ({} duplicate(s) grouped), {} forgotten as too old, in {:.1}s",
        report.sources_polled,
        report.sources_mapped,
        report.sources_by_address.len(),
        names(&report.sources_by_address),
        report.sources_feed_only.len(),
        names(&report.sources_feed_only),
        report.sources_failed,
        report.candidates,
        report.already_known,
        report.out_of_range,
        report.fetch_failed,
        report.rejected,
        report.off_topic,
        report.stored,
        report.duplicates,
        report.pruned,
        started.elapsed().as_secs_f64(),
    );
    Ok(())
}

fn summarize(args: &[String]) -> AppResult<()> {
    let opts = Options::parse(args)?;
    let config = load_config(&opts)?;
    let store = Store::open(&paths::database_path()?)?;
    let cancel = Cancel::new();

    let target = opts.lang.unwrap_or(config.settings.output_lang);
    let limit = opts.limit.unwrap_or(20);
    let mode = opts.mode.unwrap_or(Mode::TwoPass);

    let engine = Engine::new();
    let started = Instant::now();
    let model_id = prepare_engine(&engine, &config, &cancel)?;
    match config.settings.provider {
        Provider::Local => println!(
            "loaded {model_id} in {:.1}s — mode: {}",
            started.elapsed().as_secs_f64(),
            mode.as_str()
        ),
        Provider::OpenAi => println!(
            "using {} — mode: {}\nthe text of every article will be sent there",
            model_id,
            mode.as_str()
        ),
    }

    let started = Instant::now();
    let report = pipeline::summarize_pending(
        &store,
        &engine,
        &model_id,
        target,
        &config.settings.keywords,
        mode,
        limit,
        &Selection::from_config(&config),
        &cancel,
        |title, done, total| {
            let short: String = title.chars().take(60).collect();
            println!("  [{done}/{total}] {short}");
        },
        || {},
    );

    // Free the weights before reporting, so the numbers appear after the
    // memory is actually back — and so an error on the way out still unloads.
    engine.unload();
    store.checkpoint()?;
    let report = report?;

    println!(
        "\n{} summarized, {} not an article, {} failed{} in {:.1}s",
        report.summarized,
        report.not_an_article,
        report.failed,
        if report.cancelled { ", cancelled" } else { "" },
        started.elapsed().as_secs_f64(),
    );
    if let Some(e) = &report.last_failure {
        println!("last failure: {e}");
    }
    Ok(())
}

fn show(args: &[String]) -> AppResult<()> {
    let opts = Options::parse(args)?;
    let config = load_config(&opts)?;
    let store = Store::open(&paths::database_path()?)?;
    let target = opts.lang.unwrap_or(config.settings.output_lang);
    let limit = opts.limit.unwrap_or(20);
    let mode = opts.mode.unwrap_or(Mode::TwoPass);

    let cards = store.recent_summaries(
        target,
        &prompt_version(mode),
        limit,
        0,
        &Selection::from_config(&config),
    )?;
    if cards.is_empty() {
        println!("nothing for mode '{}' yet — run `fetch`, then `summarize`", mode.as_str());
        return Ok(());
    }

    for card in cards {
        println!("\n{}", "─".repeat(72));
        println!("\x1b[1m{}\x1b[0m", card.title);
        let when = card
            .published_at
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| "date unknown".into());
        println!("\x1b[2m{}  ·  {}\x1b[0m", card.source_label.unwrap_or_default(), when);
        println!();
        for bullet in &card.bullets {
            println!("  • {bullet}");
        }
        if !card.tags.is_empty() {
            println!("\n  \x1b[2m{}\x1b[0m", card.tags.join(" · "));
        }
        println!("  \x1b[2m{}\x1b[0m", card.url);
    }
    println!("\n{}", "─".repeat(72));
    Ok(())
}

fn recheck(args: &[String]) -> AppResult<()> {
    let opts = Options::parse(args)?;
    let config = load_config(&opts)?;
    let store = Store::open(&paths::database_path()?)?;
    let cancel = Cancel::new();

    // `--repromote` lets a corrected gate release what it wrongly rejected.
    let repromote = args.iter().any(|a| a == "--repromote");

    println!("re-running the quality gates over stored articles…");
    let report = pipeline::recheck(&config, &store, repromote, &cancel);
    store.checkpoint()?;
    let report = report?;

    println!(
        "{} examined, {} reclassified, {} stale summary/summaries removed",
        report.examined, report.changed, report.summaries_dropped
    );
    Ok(())
}

/// Forget what is too old to be worth keeping.
///
/// Also runs at the end of every collection, so this exists for the two cases
/// that are not that: shrinking the file after the setting is lowered, and
/// answering "is it going to grow forever" with a number.
fn prune(args: &[String]) -> AppResult<()> {
    let opts = Options::parse(args)?;
    let config = load_config(&opts)?;
    let path = paths::database_path()?;
    let store = Store::open(&path)?;

    let before = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    let was = before(&path);

    let Some(cutoff) = config.prune_before() else {
        println!("keep_days is 0 — nothing is ever forgotten");
        return Ok(());
    };

    let report = store.prune(cutoff)?;
    store.compact()?;
    println!(
        "forgot {} article(s) published before {}, with {} summary/summaries and {} recording(s)",
        report.articles,
        cutoff.format("%Y-%m-%d"),
        report.summaries,
        report.recordings
    );
    println!("database: {:.1} MB → {:.1} MB", was as f64 / 1e6, before(&path) as f64 / 1e6);
    Ok(())
}

/// Read something aloud into a WAV file.
///
/// Takes the text on the command line, or the most recent summaries when none
/// is given — which is what makes the
/// output worth listening to as a judgment of quality.
fn speak(args: &[String]) -> AppResult<()> {
    let opts = Options::parse(args)?;
    let config = load_config(&opts)?;
    let target = opts.lang.unwrap_or(config.settings.output_lang);

    let text = match opts.positional.clone() {
        Some(given) => given,
        None => {
            let store = Store::open(&paths::database_path()?)?;
            let cards = store.recent_summaries(
                target,
                &crate::llm::prompt_version(crate::llm::Mode::TwoPass),
                opts.limit.unwrap_or(3),
                0,
                &Selection::from_config(&config),
            )?;
            if cards.is_empty() {
                return Err(AppError::Other(
                    "nothing to read — run `fetch`, then `summarize`".into(),
                ));
            }
            cards
                .iter()
                .map(|c| {
                    let mut piece = c.title.trim_end_matches('.').to_string();
                    piece.push_str(". ");
                    piece.push_str(&c.bullets.join(" "));
                    piece
                })
                .collect::<Vec<_>>()
                .join("\n\n")
        }
    };

    let engine = opts.engine.unwrap_or_default();
    if let Some(caveat) = engine.caveat() {
        println!("engine: {} — {caveat}", engine.as_str());
    }

    let voice = crate::tts::Voice::new();
    if engine == crate::tts::Engine::Local {
        let model = pick_voice(&paths::voices_dir()?, target, opts.voice.as_deref())?;
        println!("loading {}…", model.file_name().unwrap_or_default().to_string_lossy());
        voice.load(&model)?;
    }

    let started = Instant::now();
    let (samples, rate) = crate::tts::speak(
        engine,
        &voice,
        &text,
        target,
        opts.voice.as_deref(),
        opts.rate.unwrap_or(1.0),
    )?;
    voice.unload();

    // With no file asked for, read it aloud instead of writing it: that is
    // what the command is for, and it exercises the same path the window uses.
    let Some(out) = opts.out.clone() else {
        let player = crate::tts::Player::new();
        let seconds = samples.len() as f32 / rate as f32;
        println!("reading aloud, {seconds:.1}s…");
        player.play(0, &samples, rate)?;
        player.wait(std::time::Duration::from_secs_f32(seconds + 2.0));
        return Ok(());
    };
    crate::tts::voice::write_wav(&out, &samples, rate)?;

    let seconds = samples.len() as f32 / rate as f32;
    println!(
        "{} — {seconds:.1}s of audio in {:.1}s ({:.0}x faster than real time)",
        out.display(),
        started.elapsed().as_secs_f32(),
        seconds / started.elapsed().as_secs_f32().max(0.001),
    );
    Ok(())
}

/// An installed voice for `lang`, by name if one was asked for.
///
/// `name` matches loosely — "irina" finds ru_RU-irina-medium — because the full
/// file name is not a thing to type.
pub fn pick_voice(dir: &std::path::Path, lang: Lang, name: Option<&str>) -> AppResult<PathBuf> {
    let prefix = format!("{}_", lang.code());
    let mut installed: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| AppError::Other(format!("{}: {e}", dir.display())))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "onnx"))
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(&prefix)))
        .collect();
    installed.sort();

    if let Some(name) = name {
        let wanted = name.to_lowercase();
        return installed
            .into_iter()
            .find(|p| {
                p.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().contains(&wanted))
            })
            .ok_or_else(|| {
                AppError::told_about(
                    "no_such_voice_installed",
                    format!("no voice matching '{name}' in {}", dir.display()),
                    name,
                )
            });
    }

    installed
        .into_iter()
        .next()
        // The window turns this into a sentence naming the screen that fixes
        // it. The command line keeps the path, because a path is what its
        // reader can act on.
        .ok_or_else(|| {
            AppError::told_about(
                "no_voice",
                format!("no {lang} voice installed in {}", dir.display()),
                lang.name(),
            )
        })
}

async fn model(args: &[String]) -> AppResult<()> {
    let config = Config::load_or_init(&paths::sources_path()?)?;
    let dir = paths::ensure(paths::models_dir_or(config.settings.models_dir.as_deref())?)?;
    let cancel = Cancel::new();

    match args.first().map(String::as_str) {
        Some("list") | None => {
            let hw = Hardware::probe();
            let suggested = registry::recommend(hw.vram_mb, hw.ram_mb);
            for m in registry::catalog() {
                let present = dir.join(&m.filename).exists();
                println!(
                    "  {} {:<14} {:>5.0}B {:<7} {:>5.1} GB download {:>6.1} GB in memory{}",
                    if present { "✓" } else { " " },
                    m.id,
                    m.params_b,
                    m.quant.label(),
                    m.size_bytes as f64 / 1e9,
                    m.memory_required_mb as f64 / 1024.0,
                    if m.id == suggested.id { "  ← suggested here" } else { "" }
                );
            }
            Ok(())
        }
        Some("download") => {
            let id = args.get(1).cloned().or_else(|| config.settings.model.clone());
            let info = match id {
                Some(id) => registry::by_id(&id)
                    .ok_or_else(|| AppError::Config(format!("unknown model '{id}'")))?,
                None => {
                    let hw = Hardware::probe();
                    registry::recommend(hw.vram_mb, hw.ram_mb)
                }
            };

            downloader::clean_partials(&dir).await?;
            println!("downloading {} ({:.1} GB)…", info.name, info.size_bytes as f64 / 1e9);

            let path = downloader::download(
                &info.url,
                &dir,
                &info.filename,
                info.sha256.as_deref(),
                &cancel,
                |p| {
                    let speed = match p.bytes_per_second {
                        0 => String::new(),
                        rate => format!("  {:.1} MB/s", rate as f64 / 1e6),
                    };
                    print!("\r  {:?} {:>3}%{speed}   ", p.phase, p.percent());
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                },
            )
            .await?;
            println!("\nready: {}", path.display());
            Ok(())
        }
        Some(other) => Err(AppError::Other(format!("unknown model subcommand '{other}'"))),
    }
}

// ---------------------------------------------------------------------------
// shared helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Options {
    days: Option<i64>,
    limit: Option<usize>,
    lang: Option<Lang>,
    keywords: Vec<String>,
    mode: Option<Mode>,
    model: Option<String>,
    out: Option<PathBuf>,
    rate: Option<f32>,
    voice: Option<String>,
    engine: Option<crate::tts::Engine>,
    /// The first argument that is not a flag or a flag's value.
    ///
    /// Collected during parsing: a second pass would need its own list of which
    /// flags take a value, and that list goes stale when a flag is added.
    positional: Option<String>,
}

impl Options {
    fn parse(args: &[String]) -> AppResult<Options> {
        let mut opts = Options::default();
        let mut i = 0;
        while i < args.len() {
            let value = |i: usize| -> AppResult<String> {
                args.get(i + 1)
                    .cloned()
                    .ok_or_else(|| AppError::Other(format!("{} needs a value", args[i])))
            };
            match args[i].as_str() {
                "--days" | "-d" => {
                    opts.days = Some(
                        value(i)?
                            .parse()
                            .map_err(|_| AppError::Other("--days wants a number".into()))?,
                    );
                    i += 1;
                }
                "--limit" | "-n" => {
                    opts.limit = Some(
                        value(i)?
                            .parse()
                            .map_err(|_| AppError::Other("--limit wants a number".into()))?,
                    );
                    i += 1;
                }
                "--lang" | "-l" => {
                    opts.lang = Some(value(i)?.parse().map_err(AppError::Other)?);
                    i += 1;
                }
                "--model" => {
                    opts.model = Some(value(i)?);
                    i += 1;
                }
                "--mode" | "-m" => {
                    opts.mode = Some(value(i)?.parse().map_err(AppError::Other)?);
                    i += 1;
                }
                "--kw" | "-k" => {
                    opts.keywords.extend(
                        value(i)?
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty()),
                    );
                    i += 1;
                }
                "--out" | "-o" => {
                    opts.out = Some(PathBuf::from(value(i)?));
                    i += 1;
                }
                "--engine" => {
                    opts.engine = Some(value(i)?.parse().map_err(AppError::Other)?);
                    i += 1;
                }
                "--voice" => {
                    opts.voice = Some(value(i)?);
                    i += 1;
                }
                "--rate" => {
                    opts.rate = Some(
                        value(i)?
                            .parse()
                            .map_err(|_| AppError::Other("--rate wants a number".into()))?,
                    );
                    i += 1;
                }
                "--repromote" => {}
                other if other.starts_with('-') => {
                    return Err(AppError::Other(format!("unknown option '{other}'")));
                }
                text => {
                    if opts.positional.is_none() {
                        opts.positional = Some(text.to_string());
                    }
                }
            }
            i += 1;
        }
        Ok(opts)
    }
}

/// Load the config, letting command-line options override the file.
fn load_config(opts: &Options) -> AppResult<Config> {
    let mut config = Config::load_or_init(&paths::sources_path()?)?;
    if let Some(lang) = opts.lang {
        config.settings.output_lang = lang;
    }
    if !opts.keywords.is_empty() {
        config.settings.keywords = opts.keywords.clone();
    }
    if let Some(model) = &opts.model {
        config.settings.model = Some(model.clone());
    }
    Ok(config)
}

/// What this configuration's model is called, without loading anything.
///
/// The same name a run files its work under, so that counts of pending work
/// and the run itself agree about which articles are still to be done.
pub fn model_id(config: &Config) -> AppResult<String> {
    match config.settings.provider {
        Provider::Local => Ok(resolve_model(config)?
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unknown".into())),
        Provider::OpenAi => {
            let chosen = config.chosen_service().ok_or_else(|| {
                AppError::told("no_service_chosen", "no service is chosen to write the summaries")
            })?;
            Ok(crate::llm::remote::Remote::new(&chosen.url, chosen.key.as_deref(), &chosen.model)?
                .id())
        }
    }
}

pub fn prepare_engine(engine: &Engine, config: &Config, cancel: &Cancel) -> AppResult<String> {
    match config.settings.provider {
        Provider::Local => {
            let path = resolve_model(config)?;
            let id = path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into());
            engine.load(&path, cancel)?;
            Ok(id)
        }
        Provider::OpenAi => {
            // The key comes from the saved entry rather than from the settings:
            // the settings only name which entry is in use.
            let chosen = config.chosen_service().ok_or_else(|| {
                AppError::told("no_service_chosen", "no service is chosen to write the summaries")
            })?;
            let service =
                crate::llm::remote::Remote::new(&chosen.url, chosen.key.as_deref(), &chosen.model)?;
            let id = service.id();
            engine.use_service(service);
            Ok(id)
        }
    }
}

/// Find the GGUF to load: an explicit path, a catalog id, or whatever the
/// hardware suggests — and say plainly what to run if it is not there yet.
pub fn resolve_model(config: &Config) -> AppResult<PathBuf> {
    let dir = paths::models_dir_or(config.settings.models_dir.as_deref())?;

    if let Some(setting) = &config.settings.model {
        let as_path = PathBuf::from(setting);
        if as_path.is_file() {
            return Ok(as_path);
        }
        if let Some(info) = registry::by_id(setting) {
            let path = dir.join(&info.filename);
            if path.is_file() {
                return Ok(path);
            }
            // Chosen, but never fetched. Not the same as "no model at all",
            // and the window has different words for each.
            return Err(AppError::told_about(
                "model_missing",
                format!("model '{setting}' is not downloaded yet"),
                info.name.clone(),
            ));
        }
        return Err(AppError::Config(format!(
            "model '{setting}' is neither a file nor a known id"
        )));
    }

    // Nothing configured: take whatever is already downloaded, preferring the
    // largest, before suggesting a download.
    let mut present: Vec<_> =
        registry::catalog().into_iter().filter(|m| dir.join(&m.filename).is_file()).collect();
    present.sort_by_key(|m| m.memory_required_mb);

    if let Some(best) = present.pop() {
        return Ok(dir.join(best.filename));
    }

    let hw = Hardware::probe();
    let suggested = registry::recommend(hw.vram_mb, hw.ram_mb);
    // The English text names a command because the command line is where it is
    // read. The window has its own words for this code, and they name a button
    // — telling someone who installed a program to open a terminal is not an
    // answer.
    Err(AppError::told_about(
        "no_model",
        format!(
            "no model on disk — run `shadow-tentacles model download {}` ({:.1} GB)",
            suggested.id,
            suggested.size_bytes as f64 / 1e9
        ),
        suggested.name.clone(),
    ))
}

fn print_usage() {
    println!(
        "shadow-tentacles {}\n\
\n\
USAGE\n    shadow-tentacles              Open the window\n    shadow-tentacles <command>    Run headless\n\
\n\
COMMANDS\n\
    fetch        Poll every source and store what is new\n\
    summarize    Summarize stored articles with the local model\n\
    show         Print the summaries collected so far\n\
    recheck      Re-judge stored articles after the quality rules change\n\
    prune        Forget stored articles older than keep_days, and compact\n\
    speak        Read the latest summaries, or given text, into a WAV file\n\
\x20                (add --repromote to also release what a corrected gate rejected)\n\
    run          fetch, then summarize, then show\n\
    sources      List the configured sources\n\
    model        list | download [id]\n\
    doctor       Hardware, paths and what is in the database\n\
\n\
OPTIONS\n\
    -d, --days N     Only articles published in the last N days\n\
    -n, --limit N    Stop after N articles (default 20)\n\
    -l, --lang CODE  Output language: ru, en, de, fr, es, pt\n\
    -k, --kw a,b,c   Keywords; prefix one with '-' to exclude\n\
    -m, --mode M     direct (one pass) or 2pass (summarize, then translate)\n\
    -o, --out FILE   Where to write the audio (speak)\n\
        --rate N     Speaking pace, 1.0 is the voice's own (speak)\n\
        --voice NAME Which voice, e.g. irina or ru-RU-SvetlanaNeural (speak)\n\
        --engine E   local (private, always works) or online (better voice,\n\
    \x20                sends the text to Microsoft) (speak)\n\
        --model M    Catalog id or path to a .gguf, overriding the config\n",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Options {
        Options::parse(&args.iter().map(|a| a.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn a_flags_value_is_never_mistaken_for_the_text() {
        // The bug this replaced: a second pass over the arguments had its own
        // list of which flags take a value, and adding --engine without adding
        // it there made `speak --engine online "text"` read the word "online".
        let opts = parse(&["--lang", "ru", "--engine", "online", "-o", "/tmp/a.wav", "прочти это"]);
        assert_eq!(opts.positional.as_deref(), Some("прочти это"));
        assert_eq!(opts.lang, Some(Lang::Ru));
        assert_eq!(opts.engine, Some(crate::tts::Engine::Online));
    }

    #[test]
    fn no_text_means_no_positional() {
        assert_eq!(parse(&["--lang", "en", "--limit", "5"]).positional, None);
    }

    #[test]
    fn the_first_free_argument_wins() {
        let opts = parse(&["первое", "второе"]);
        assert_eq!(opts.positional.as_deref(), Some("первое"));
    }

    #[test]
    fn an_unknown_option_is_refused_not_ignored() {
        let args = ["--nonsense".to_string()];
        assert!(Options::parse(&args).is_err());
    }
}
