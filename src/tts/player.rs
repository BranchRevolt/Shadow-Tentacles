// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Playing the audio in-process rather than in the window.
//!
//! On Linux an `<audio>` element is backed by GStreamer, whose WAV plugin lives
//! in a package a desktop need not have.

use std::io::Cursor;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
// rodio 0.22 calls a sink a `Player`, which is also this type's name
// that owns one; aliased so the two never have to be told apart by context.
use rodio::Player as Sink;
use rodio::stream::{DeviceSinkBuilder, MixerDeviceSink};

use crate::error::{AppError, AppResult};

struct Playing {
    sink: Sink,
    /// Kept alive for as long as the sink is: dropping the device stops the
    /// sound, however much audio is still queued.
    _device: MixerDeviceSink,
    /// How much has been heard, not counting the stretch in progress.
    played: Duration,
    /// When the stretch in progress began, or `None` while paused.
    ///
    /// Wall time since the start would keep running through a pause, which the
    /// window would read as the recording having finished.
    since: Option<Instant>,
    duration: f32,
    /// Which card is being read, so the window can show it on the right one.
    card: i64,
}

impl Playing {
    fn position(&self) -> f32 {
        let running = self.since.map(|at| at.elapsed()).unwrap_or_default();
        (self.played + running).as_secs_f32().min(self.duration)
    }
}

#[derive(Default)]
pub struct Player {
    current: Mutex<Option<Playing>>,
}

/// What the window needs to draw the control.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Playback {
    pub card: Option<i64>,
    pub playing: bool,
    pub position: f32,
    pub duration: f32,
}

impl Player {
    pub fn new() -> Arc<Player> {
        Arc::new(Player::default())
    }

    /// Start reading a recording from disk, replacing anything already playing.
    ///
    /// Played straight from the file: rodio decodes it, so no copy of the whole
    /// recording is made.
    pub fn play_file(&self, card: i64, path: &std::path::Path, duration: f32) -> AppResult<()> {
        let file = std::fs::File::open(path)
            .map_err(|e| AppError::Other(format!("{}: {e}", path.display())))?;
        let decoder = rodio::Decoder::new(std::io::BufReader::new(file))
            .map_err(|e| AppError::Other(format!("{} is not playable: {e}", path.display())))?;
        self.start(card, duration, decoder)
    }

    /// Start reading `samples`, replacing anything already playing.
    ///
    /// Replacing rather than queueing: pressing play on another card means
    /// "read this one instead", and two summaries at once is nobody's intent.
    pub fn play(&self, card: i64, samples: &[f32], rate: u32) -> AppResult<()> {
        let mut wav = Vec::new();
        write_wav_to(&mut wav, samples, rate)?;
        let decoder = rodio::Decoder::new(Cursor::new(wav))
            .map_err(|e| AppError::Other(format!("could not read back the audio: {e}")))?;
        self.start(card, samples.len() as f32 / rate as f32, decoder)
    }

    fn start<S>(&self, card: i64, duration: f32, source: S) -> AppResult<()>
    where
        S: rodio::Source + Send + 'static,
    {
        let mut device = DeviceSinkBuilder::open_default_sink()
            .map_err(|e| AppError::Other(format!("no sound output available: {e}")))?;
        // Dropping the device is what stops playback, so its complaint about being
        // dropped mid-sound is describing the intended behavior, not a fault.
        device.log_on_drop(false);
        let sink = Sink::connect_new(device.mixer());
        sink.append(source);
        sink.play();

        *self.current.lock() = Some(Playing {
            sink,
            _device: device,
            played: Duration::ZERO,
            since: Some(Instant::now()),
            duration,
            card,
        });
        Ok(())
    }

    pub fn pause(&self) {
        if let Some(p) = self.current.lock().as_mut() {
            // Bank what has been heard and stop the clock, or the position
            // goes on climbing through the silence.
            if let Some(at) = p.since.take() {
                p.played += at.elapsed();
            }
            p.sink.pause();
        }
    }

    pub fn resume(&self) {
        if let Some(p) = self.current.lock().as_mut() {
            if p.since.is_none() {
                p.since = Some(Instant::now());
            }
            p.sink.play();
        }
    }

    pub fn stop(&self) {
        // Dropping the sink and its stream is what silences it; pausing would
        // leave the device open with nothing playing through it.
        *self.current.lock() = None;
    }

    /// Where playback is, for the window to draw.
    pub fn state(&self) -> Playback {
        let mut guard = self.current.lock();
        let Some(p) = guard.as_ref() else {
            return Playback { card: None, playing: false, position: 0.0, duration: 0.0 };
        };

        if p.sink.empty() {
            // Finished on its own; forget it so the next state is honest.
            let card = p.card;
            let duration = p.duration;
            *guard = None;
            return Playback { card: Some(card), playing: false, position: duration, duration };
        }

        Playback {
            card: Some(p.card),
            // Our own bookkeeping rather than the sink's: the clock and the
            // flag have to agree, and they only do when one thing decides both.
            playing: p.since.is_some(),
            position: p.position(),
            duration: p.duration,
        }
    }

    /// True while something is being read.
    pub fn is_playing(&self) -> bool {
        self.current.lock().as_ref().is_some_and(|p| !p.sink.empty() && p.since.is_some())
    }

    /// Block until the reading finishes or `timeout` passes.
    ///
    /// Used on the way out: a process that exits while the sound card is mid
    /// sentence cuts the audio off, which sounds like a crash.
    pub fn wait(&self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while self.is_playing() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

/// Write samples as a WAV into a buffer.
fn write_wav_to(out: &mut Vec<u8>, samples: &[f32], rate: u32) -> AppResult<()> {
    use std::io::Write;

    let pcm: Vec<u8> = samples
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
        .collect();
    let bytes = pcm.len() as u32;

    out.write_all(b"RIFF")?;
    out.write_all(&(36 + bytes).to_le_bytes())?;
    out.write_all(b"WAVEfmt ")?;
    out.write_all(&16u32.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?;
    out.write_all(&1u16.to_le_bytes())?;
    out.write_all(&rate.to_le_bytes())?;
    out.write_all(&(rate * 2).to_le_bytes())?;
    out.write_all(&2u16.to_le_bytes())?;
    out.write_all(&16u16.to_le_bytes())?;
    out.write_all(b"data")?;
    out.write_all(&bytes.to_le_bytes())?;
    out.write_all(&pcm)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_playing_is_reported_as_nothing() {
        let player = Player::new();
        let state = player.state();
        assert_eq!(state.card, None);
        assert!(!state.playing);
        assert!(!player.is_playing());
    }

    #[test]
    fn stopping_when_idle_is_not_an_error() {
        let player = Player::new();
        player.stop();
        player.pause();
        player.resume();
        assert!(!player.is_playing());
    }

    #[test]
    fn the_buffer_is_a_wav_of_the_right_length() {
        let mut buffer = Vec::new();
        write_wav_to(&mut buffer, &[0.0, 0.5, -0.5], 22050).unwrap();
        assert_eq!(&buffer[0..4], b"RIFF");
        assert_eq!(buffer.len(), 44 + 6);
    }

    #[test]
    fn waiting_when_nothing_plays_returns_at_once() {
        let player = Player::new();
        let started = Instant::now();
        player.wait(Duration::from_secs(5));
        assert!(started.elapsed() < Duration::from_millis(200));
    }
}
