// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Resident llama.cpp engine: load once, generate many times, unload on demand.
//!
//! VRAM is released by `LlamaModel::drop`; see `unload` for how that interacts
//! with a generation still in flight.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Instant;

use llama_cpp_2::TokenToStringError;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
use llama_cpp_2::sampling::LlamaSampler;
use parking_lot::Mutex;

use crate::error::{AppError, AppResult};
use crate::shutdown::Cancel;

/// The llama.cpp backend is a process global: initializing it twice is an error
/// in llama-cpp-2, and freeing it while a model is alive is undefined behavior.
/// Initialized once and never dropped; it holds no VRAM.
static BACKEND: OnceLock<LlamaBackend> = OnceLock::new();

fn backend() -> AppResult<&'static LlamaBackend> {
    if let Some(b) = BACKEND.get() {
        return Ok(b);
    }

    // llama.cpp and ggml write pages of load dumps and per-graph detail straight
    // to stderr, which buries the program's own output. Routing them into
    // tracing rather than switching them off means the EnvFilter drops them by
    // default while `RUST_LOG=llama_cpp_2=debug` still brings them back when
    // something needs diagnosing.
    llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default());

    let b = LlamaBackend::init().map_err(|e| AppError::Inference(e.to_string()))?;
    // Another thread may have won the race; either way one backend results.
    Ok(BACKEND.get_or_init(|| b))
}

pub struct GenOptions {
    pub n_threads: i32,
    pub ctx_size: u32,
    pub max_new_tokens: i32,
    pub temp: f32,
    /// 1.0 = off. High values push the model to avoid words already in context,
    /// which on a short Russian summary makes it code-switch into English.
    /// Moderate is enough to stop the greedy repetition loop.
    pub repeat_penalty: f32,
    pub penalty_last_n: i32,
}

impl Default for GenOptions {
    fn default() -> Self {
        Self {
            n_threads: std::thread::available_parallelism()
                .map(|n| n.get().min(8) as i32)
                .unwrap_or(4),
            ctx_size: 8192,
            max_new_tokens: 768,
            temp: 0.5,
            repeat_penalty: 1.15,
            penalty_last_n: 128,
        }
    }
}

struct Loaded {
    path: PathBuf,
    model: LlamaModel,
}

/// Where the summaries come from.
///
/// An enum rather than a trait object: callers ask for two things only — a
/// token count and a generation — and only this file needs to know which case
/// is in play.
enum Backend {
    /// A GGUF resident in this process, holding VRAM until it is unloaded.
    Here(Loaded),
    /// A service somewhere else. Nothing is held; every call is a request.
    Away(crate::llm::remote::Remote),
}

/// Holds whatever writes the summaries. Cheap to clone-share behind an `Arc`.
pub struct Engine {
    inner: Mutex<Option<Backend>>,
}

impl Engine {
    pub fn new() -> Self {
        Self { inner: Mutex::new(None) }
    }

    /// Whether a model is resident in this machine's memory.
    ///
    /// A service is never "loaded": there is nothing here to unload, and the
    /// window's unload button follows this.
    pub fn is_loaded(&self) -> bool {
        matches!(*self.inner.lock(), Some(Backend::Here(_)))
    }

    /// Point the engine at a service. Replaces whatever was loaded, because
    /// holding a GGUF for summaries that will be written elsewhere is rude on
    /// a machine that has other work.
    pub fn use_service(&self, remote: crate::llm::remote::Remote) {
        let mut guard = self.inner.lock();
        if matches!(*guard, Some(Backend::Here(_))) {
            tracing::info!("switching to a service, unloading the local model");
        }
        *guard = Some(Backend::Away(remote));
    }

    /// The file a resident model came from. `None` for a service, which has no
    /// file — the caller asking this wants to know what is in memory.
    pub fn loaded_path(&self) -> Option<PathBuf> {
        match &*self.inner.lock() {
            Some(Backend::Here(l)) => Some(l.path.clone()),
            _ => None,
        }
    }

    /// Load `path`, or do nothing if that exact file is already resident.
    /// Switching models unloads the old one first.
    ///
    /// Loading cannot be interrupted once started, the wrapper not exposing
    /// llama.cpp's abort callback, so cancellation is checked only before it.
    pub fn load(&self, path: &Path, cancel: &Cancel) -> AppResult<()> {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        let mut guard = self.inner.lock();
        match guard.as_ref() {
            Some(Backend::Here(l)) if l.path == path => return Ok(()),
            Some(Backend::Here(l)) => {
                tracing::info!("switching model, unloading {}", l.path.display());
                *guard = None; // drop the old model before allocating the new one
            }
            // A service was in use and a local model has been asked for.
            Some(Backend::Away(_)) => *guard = None,
            None => {}
        }

        let backend = backend()?;
        // Offload every layer to the GPU; llama clamps to the model's real layer
        // count, so an over-large value simply means "all of them". On a CPU-only
        // build this is ignored.
        let params = LlamaModelParams::default().with_n_gpu_layers(999);

        let started = Instant::now();
        let model = LlamaModel::load_from_file(backend, path, &params)
            .map_err(|e| AppError::Inference(format!("failed to load {}: {e}", path.display())))?;
        tracing::info!("model loaded in {:?}: {}", started.elapsed(), path.display());

        *guard = Some(Backend::Here(Loaded { path: path.to_path_buf(), model }));
        Ok(())
    }

    /// Drop the model and release its memory. Returns false if nothing was
    /// loaded.
    ///
    /// A generation in flight holds the lock, so this blocks until it finishes;
    /// canceling first makes it return promptly.
    pub fn unload(&self) -> bool {
        let mut guard = self.inner.lock();
        // A service is left in place: there is nothing of it in memory, and
        // dropping it would quietly turn the next summary into a refusal.
        match guard.as_ref() {
            Some(Backend::Here(_)) => {}
            _ => return false,
        }
        if let Some(Backend::Here(l)) = guard.take() {
            tracing::info!("unloading model {}", l.path.display());
            drop(l); // LlamaModel::drop is what actually frees VRAM
        }
        true
    }

    /// How long `text` is in tokens.
    ///
    /// Exact for a resident model, an estimate for a service; see
    /// `remote::token_estimate`. Tokens rather than characters because the ratio
    /// between them differs by 2-3x across the supported languages.
    pub fn token_len(&self, text: &str) -> AppResult<usize> {
        let guard = self.inner.lock();
        match guard.as_ref().ok_or(AppError::ModelNotLoaded)? {
            Backend::Here(l) => Ok(token_len(&l.model, text)),
            Backend::Away(_) => Ok(crate::llm::remote::token_estimate(text)),
        }
    }

    /// Run one generation. `system` is the instruction, `user` the data.
    pub fn generate(
        &self,
        system: &str,
        user: &str,
        opts: &GenOptions,
        cancel: &Cancel,
    ) -> AppResult<String> {
        let mut guard = self.inner.lock();
        match guard.as_mut().ok_or(AppError::ModelNotLoaded)? {
            Backend::Here(loaded) => {
                let backend = backend()?;
                generate(backend, &loaded.model, system, user, opts, cancel)
            }
            Backend::Away(service) => service.generate(system, user, opts, cancel),
        }
    }
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

fn token_len(model: &LlamaModel, text: &str) -> usize {
    model
        .str_to_token(text, AddBos::Never)
        .map(|t| t.len())
        .unwrap_or_else(|_| text.chars().count() / 3)
}

/// One generation against an already-loaded model.
///
/// Qwen3 needs ChatML framing and `/no_think`; `n_batch` must equal the context
/// or a large prompt trips `GGML_ASSERT(n_tokens <= n_batch)`; token pieces are
/// decoded once at the end so multi-byte Cyrillic is not split; the penalties
/// keep a greedy sampler off a repeating phrase.
fn generate(
    backend: &LlamaBackend,
    model: &LlamaModel,
    system: &str,
    user: &str,
    opts: &GenOptions,
    cancel: &Cancel,
) -> AppResult<String> {
    if cancel.is_cancelled() {
        return Err(AppError::Cancelled);
    }

    let prompt = format!(
        "<|im_start|>system\n{}<|im_end|>\n<|im_start|>user\n{} /no_think<|im_end|>\n<|im_start|>assistant\n",
        system.trim(),
        user.trim(),
    );

    let ctx_params = LlamaContextParams::default()
        .with_n_ctx(std::num::NonZeroU32::new(opts.ctx_size))
        .with_n_batch(opts.ctx_size)
        .with_n_threads(opts.n_threads)
        .with_n_threads_batch(opts.n_threads);

    let mut ctx =
        model.new_context(backend, ctx_params).map_err(|e| AppError::Inference(e.to_string()))?;

    // ChatML carries its own control tokens, so no BOS.
    let mut tokens = model
        .str_to_token(&prompt, AddBos::Never)
        .map_err(|e| AppError::Inference(e.to_string()))?;

    // Safety net for pathological input: chunk sizing stays well under this,
    // but degrading by truncation always beats llama.cpp aborting the process.
    let max_prompt =
        (opts.ctx_size as usize).saturating_sub(opts.max_new_tokens as usize + 16).max(16);
    if tokens.len() > max_prompt {
        tracing::warn!("prompt truncated from {} to {} tokens", tokens.len(), max_prompt);
        tokens.truncate(max_prompt);
    }

    let n_tokens = tokens.len();
    let mut batch = LlamaBatch::new(n_tokens.max(512), 1);
    for (i, &token) in tokens.iter().enumerate() {
        let is_last = i == n_tokens - 1;
        batch
            .add(token, i as i32, &[0], is_last)
            .map_err(|e| AppError::Inference(e.to_string()))?;
    }
    ctx.decode(&mut batch).map_err(|e| AppError::Inference(e.to_string()))?;

    let mut output_tokens = Vec::new();
    let mut n_cur = n_tokens as i32;
    // Qwen3's recommended non-thinking sampling (temp 0.7, top_p 0.8, top_k 20)
    // plus a repeat penalty.
    let mut sampler = LlamaSampler::chain_simple([
        LlamaSampler::penalties(opts.penalty_last_n, opts.repeat_penalty, 0.0, 0.0),
        LlamaSampler::top_k(20),
        LlamaSampler::top_p(0.8, 1),
        LlamaSampler::temp(opts.temp),
        LlamaSampler::dist(1234),
    ]);

    loop {
        // The inner loop of the whole program. Checking here is what makes a
        // quit take one token rather than one article.
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }

        let new_token_id = sampler.sample(&ctx, batch.n_tokens() - 1);
        if model.is_eog_token(new_token_id) || n_cur - n_tokens as i32 >= opts.max_new_tokens {
            break;
        }

        output_tokens.push(new_token_id);
        sampler.accept(new_token_id);

        batch.clear();
        batch
            .add(new_token_id, n_cur, &[0], true)
            .map_err(|e| AppError::Inference(e.to_string()))?;
        n_cur += 1;
        ctx.decode(&mut batch).map_err(|e| AppError::Inference(e.to_string()))?;
    }

    let mut bytes = Vec::new();
    for &tok in &output_tokens {
        let piece = match model.token_to_piece_bytes(tok, 64, false, None) {
            Ok(b) => b,
            Err(TokenToStringError::InsufficientBufferSpace(n)) => model
                .token_to_piece_bytes(tok, n.unsigned_abs() as usize, false, None)
                .map_err(|e| AppError::Inference(e.to_string()))?,
            Err(e) => return Err(AppError::Inference(e.to_string())),
        };
        bytes.extend_from_slice(&piece);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();

    // Drop a leftover <think>…</think> block if the model emitted one anyway.
    let text = match text.split_once("</think>") {
        Some((_, rest)) => rest,
        None => &text,
    };
    Ok(text.trim().to_string())
}
