// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! The synthesizer: a loaded voice, and the audio it produces.
//!
//! Load once, speak many times, release on request.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

use parking_lot::Mutex;
use piper_rs::Piper;

use crate::error::{AppError, AppResult};
use crate::llm::Lang;
use crate::tts::phonemes;
use crate::tts::stress::StressDictionary;

struct Loaded {
    path: PathBuf,
    piper: Piper,
    /// espeak's name for the dialect this voice was trained against, taken from
    /// the voice's own config rather than guessed from the language.
    espeak_voice: String,
    last_used: Instant,
}

pub struct Voice {
    inner: Mutex<Option<Loaded>>,
    /// Russian stress data, opened once and mapped for the life of the process.
    dictionary: Mutex<Option<StressDictionary>>,
}

/// How much slower than the voice's natural pace to speak, 1.0 being its own.
///
/// The voices disagree by half again on the same text while both declaring a
/// length scale of 1, so the correction cannot come from their configs.
pub const DEFAULT_RATE: f32 = 1.0;

impl Voice {
    pub fn new() -> Voice {
        Voice { inner: Mutex::new(None), dictionary: Mutex::new(None) }
    }

    pub fn is_loaded(&self) -> bool {
        self.inner.lock().is_some()
    }

    /// Load `model` (a .onnx) with its .onnx.json beside it.
    pub fn load(&self, model: &Path) -> AppResult<()> {
        let mut guard = self.inner.lock();
        if guard.as_ref().is_some_and(|l| l.path == model) {
            return Ok(());
        }
        *guard = None; // release the old voice before allocating the new one

        let config = model.with_extension("onnx.json");
        if !config.is_file() {
            return Err(AppError::Other(format!(
                "{} is missing its .onnx.json — a voice is two files",
                model.display()
            )));
        }

        let started = Instant::now();
        let espeak_voice = espeak_voice_of(&config, lang_of(model));
        let piper = Piper::new(model, &config)
            .map_err(|e| AppError::Other(format!("{}: {e}", model.display())))?;
        tracing::info!(
            "voice loaded in {:?}: {} (espeak {espeak_voice})",
            started.elapsed(),
            model.display()
        );

        *guard = Some(Loaded {
            path: model.to_path_buf(),
            piper,
            espeak_voice,
            last_used: Instant::now(),
        });
        Ok(())
    }

    pub fn unload(&self) -> bool {
        self.inner.lock().take().is_some()
    }

    /// Speak `text`, returning PCM samples and their rate.
    pub fn speak(&self, text: &str, lang: Lang, rate: f32) -> AppResult<(Vec<f32>, u32)> {
        let dictionary = self.dictionary(lang)?;

        // The voice is consulted before the text is phonemized, because which
        // dialect espeak should speak is the voice's answer, not the language's.
        let mut guard = self.inner.lock();
        let loaded = guard.as_mut().ok_or_else(|| AppError::Other("no voice loaded".into()))?;
        loaded.last_used = Instant::now();

        let ipa = phonemes::phonemise(
            text,
            lang,
            &loaded.espeak_voice,
            dictionary.as_ref().and_then(|d| d.as_ref()),
        );
        if ipa.trim().is_empty() {
            return Err(AppError::Other("nothing to read".into()));
        }

        // `true`: these are phonemes, not text. That is the whole point — it is
        // how the corrected Russian stress reaches the model at all.
        loaded
            .piper
            .create(&ipa, true, None, Some(rate), None, None)
            .map_err(|e| AppError::Other(format!("synthesis failed: {e}")))
    }

    /// Open the stress dictionary once, and only for the language that needs it.
    fn dictionary(
        &self,
        lang: Lang,
    ) -> AppResult<Option<parking_lot::MappedMutexGuard<'_, Option<StressDictionary>>>> {
        if lang != Lang::Ru {
            return Ok(None);
        }
        let mut guard = self.dictionary.lock();
        if guard.is_none() {
            let dir = crate::paths::voices_dir()?;
            if StressDictionary::is_installed(&dir) {
                *guard = Some(StressDictionary::open(&dir)?);
            } else {
                // Speaking with espeak's own stress is wrong about a third of
                // the time, but silence is worse; the caller is told elsewhere
                // that the dictionary is missing.
                tracing::warn!("no Russian stress dictionary; pronunciation will be poor");
            }
        }
        Ok(Some(parking_lot::MutexGuard::map(guard, |g| g)))
    }
}

/// What espeak voice this Piper model was trained against.
///
/// Only the model's own config knows: a file named `en_GB` wants espeak's
/// `en-gb-x-rp`, which the language code alone does not give. A config that
/// does not say falls back to the language code.
fn espeak_voice_of(config: &Path, fallback: &str) -> String {
    let read = || -> Option<String> {
        let text = std::fs::read_to_string(config).ok()?;
        let json: serde_json::Value = serde_json::from_str(&text).ok()?;
        let voice = json.get("espeak")?.get("voice")?.as_str()?.trim();
        (!voice.is_empty()).then(|| voice.to_string())
    };
    read().unwrap_or_else(|| {
        tracing::warn!("{} names no espeak voice; falling back to {fallback}", config.display());
        fallback.to_string()
    })
}

/// The language code in a Piper file name: `pt_BR-faber-medium` is `pt`.
///
/// Only ever the fallback for a config that does not name its espeak voice, so
/// a name in some other shape gives English rather than an error.
fn lang_of(model: &Path) -> &'static str {
    let name = model.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Lang::ALL
        .into_iter()
        .find(|l| name.starts_with(&format!("{}_", l.code())))
        .map(Lang::code)
        .unwrap_or("en")
}

impl Default for Voice {
    fn default() -> Voice {
        Voice::new()
    }
}

/// Write 16-bit PCM samples as a WAV file.
///
/// Written by hand rather than with a crate: the header is forty-four bytes of
/// well-documented constants, and this is the only audio format produced here.
pub fn write_wav(path: &Path, samples: &[f32], rate: u32) -> AppResult<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
        .collect();
    let bytes = pcm.len() as u32;

    let mut file = std::io::BufWriter::new(std::fs::File::create(path)?);
    file.write_all(b"RIFF")?;
    file.write_all(&(36 + bytes).to_le_bytes())?;
    file.write_all(b"WAVEfmt ")?;
    file.write_all(&16u32.to_le_bytes())?; // PCM header length
    file.write_all(&1u16.to_le_bytes())?; // uncompressed
    file.write_all(&1u16.to_le_bytes())?; // mono
    file.write_all(&rate.to_le_bytes())?;
    file.write_all(&(rate * 2).to_le_bytes())?; // bytes per second
    file.write_all(&2u16.to_le_bytes())?; // bytes per frame
    file.write_all(&16u16.to_le_bytes())?; // bits per sample
    file.write_all(b"data")?;
    file.write_all(&bytes.to_le_bytes())?;
    file.write_all(&pcm)?;
    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_voice_is_read_with_the_dialect_it_was_trained_on() {
        // Measured against the published voices: en_GB-alan declares
        // "en-gb-x-rp", es_MX-claude declares "es-419", pt_BR-faber declares
        // "pt-br". Before this, every one of them was phonemized as the bare
        // language code — which for the British voice meant American vowels.
        let dir = std::env::temp_dir().join(format!("shadow-espeak-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let config = dir.join("en_GB-alan-medium.onnx.json");
        std::fs::write(&config, r#"{"espeak": {"voice": "en-gb-x-rp"}}"#).unwrap();
        assert_eq!(espeak_voice_of(&config, "en"), "en-gb-x-rp");

        // A config that says nothing falls back to the language in the name,
        // as was done for every voice before.
        let silent = dir.join("pt_BR-faber-medium.onnx.json");
        std::fs::write(&silent, "{}").unwrap();
        assert_eq!(espeak_voice_of(&silent, lang_of(&silent)), "pt");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_language_in_a_voices_name_is_the_last_resort() {
        assert_eq!(lang_of(Path::new("ru_RU-irina-medium.onnx")), "ru");
        assert_eq!(lang_of(Path::new("es_MX-ald-medium.onnx")), "es");
        assert_eq!(lang_of(Path::new("pt_BR-faber-medium.onnx")), "pt");
        // Something in another shape gives an answer rather than an error: a
        // voice a user put there by hand should still speak.
        assert_eq!(lang_of(Path::new("my-own-voice.onnx")), "en");
    }

    #[test]
    fn a_wav_carries_the_right_header_and_length() {
        let dir = std::env::temp_dir().join(format!("shadow-wav-{}", std::process::id()));
        let path = dir.join("a.wav");
        write_wav(&path, &[0.0, 0.5, -0.5, 1.0], 22050).unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        // Four samples, sixteen bits each.
        assert_eq!(bytes.len(), 44 + 8);
        assert_eq!(u32::from_le_bytes(bytes[40..44].try_into().unwrap()), 8);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn samples_beyond_the_range_are_clamped_not_wrapped() {
        // A value above 1.0 cast straight to i16 wraps to a loud click.
        let dir = std::env::temp_dir().join(format!("shadow-clamp-{}", std::process::id()));
        let path = dir.join("b.wav");
        write_wav(&path, &[2.0, -2.0], 22050).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(i16::from_le_bytes(bytes[44..46].try_into().unwrap()), i16::MAX);
        assert_eq!(i16::from_le_bytes(bytes[46..48].try_into().unwrap()), -i16::MAX);
        std::fs::remove_dir_all(&dir).ok();
    }
}
