// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Microsoft Edge's reading service, reached the way the browser reaches it.
//!
//! Off by default: it sends the summary text to a third party. The endpoint is
//! undocumented and can change without notice.

use msedge_tts::tts::SpeechConfig;
use msedge_tts::tts::client::connect;

use crate::error::{AppError, AppResult};
use crate::llm::Lang;

/// MP3 at 24 kHz.
///
/// The raw PCM formats the protocol documents come back empty from this
/// endpoint: the same request returns 17856 bytes as MP3 and zero as PCM.
const AUDIO_FORMAT: &str = "audio-24khz-48kbitrate-mono-mp3";

/// A reasonable voice per language, for someone who has not chosen one.
pub fn default_voice(lang: Lang) -> &'static str {
    match lang {
        Lang::Ru => "ru-RU-SvetlanaNeural",
        Lang::En => "en-US-AriaNeural",
        Lang::De => "de-DE-KatjaNeural",
        Lang::Fr => "fr-FR-DeniseNeural",
        Lang::Es => "es-ES-ElviraNeural",
        Lang::Pt => "pt-BR-FranciscaNeural",
    }
}

/// Speak `text`, returning samples and their rate.
///
/// `rate` follows the local engine's convention, where 1.0 is the voice's own
/// pace and larger is slower. The service instead takes a percentage change,
/// with positive meaning faster, so the two are inverses of one another.
pub fn speak(text: &str, lang: Lang, voice: Option<&str>, rate: f32) -> AppResult<(Vec<f32>, u32)> {
    if text.trim().is_empty() {
        return Err(AppError::Other("nothing to read".into()));
    }

    let config = SpeechConfig {
        voice_name: voice.unwrap_or_else(|| default_voice(lang)).to_string(),
        audio_format: AUDIO_FORMAT.to_string(),
        pitch: 0,
        rate: percent_change(rate),
        volume: 0,
    };

    let mut client = connect().map_err(|e| {
        AppError::Network(format!(
            "could not reach the online reading service: {e}. It needs an internet \
             connection, and it is an undocumented endpoint that may simply have changed"
        ))
    })?;

    let audio = client
        .synthesize(text, &config)
        .map_err(|e| AppError::Network(format!("the online reading service refused: {e}")))?;

    tracing::debug!("online: {} bytes of mp3 returned", audio.audio_bytes.len());
    decode_mp3(&audio.audio_bytes)
}

/// Our speed multiplier as the percentage the service expects.
///
/// 1.0 is unchanged; 1.25 means a quarter slower, which is −20% of the original
/// pace, not −25% — the relation is reciprocal, and getting it backwards makes
/// the "slower" control speed the voice up.
fn percent_change(rate: f32) -> i32 {
    if rate <= 0.0 {
        return 0;
    }
    ((1.0 / rate - 1.0) * 100.0).round().clamp(-90.0, 200.0) as i32
}

/// Decode the returned MP3 into the samples the rest of the code works with.
///
/// The sample rate is taken from the stream rather than assumed: the service
/// names 24 kHz in the format string, and a mismatch between what it says and
/// what it sends would play back at the wrong pitch.
fn decode_mp3(bytes: &[u8]) -> AppResult<(Vec<f32>, u32)> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::DecoderOptions;
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = std::io::Cursor::new(bytes.to_vec());
    let stream = MediaSourceStream::new(Box::new(source), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");

    let probed = symphonia::default::get_probe()
        .format(&hint, stream, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| AppError::Other(format!("the returned audio is not readable: {e}")))?;

    let mut format = probed.format;
    let track = format
        .default_track()
        .ok_or_else(|| AppError::Other("the returned audio has no track".into()))?;
    let track_id = track.id;

    let mut decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| AppError::Other(format!("no decoder for the returned audio: {e}")))?;

    let mut samples: Vec<f32> = Vec::new();
    let mut rate = 0u32;

    // The loop ends when `next_packet` fails, which is how symphonia reports the
    // end of a stream: as an I/O error rather than as `None`.
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != track_id {
            continue;
        }
        let decoded = match decoder.decode(&packet) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let spec = *decoded.spec();
        rate = spec.rate;
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buffer.copy_interleaved_ref(decoded);
        // Mono was requested; if a channel count ever changes, take the
        // first channel rather than playing the interleave as though it were one.
        let channels = spec.channels.count().max(1);
        samples.extend(buffer.samples().iter().step_by(channels));
    }

    if samples.is_empty() {
        return Err(AppError::Network("the online reading service returned no audio".into()));
    }
    Ok((samples, rate))
}

/// Every voice the service offers, for a chooser that is not a hardcoded list.
pub fn available_voices() -> AppResult<Vec<(String, String)>> {
    let voices = msedge_tts::voice::get_voices_list()
        .map_err(|e| AppError::Network(format!("could not list the online voices: {e}")))?;

    Ok(voices
        .into_iter()
        .map(|v| {
            let label =
                v.friendly_name.clone().unwrap_or_else(|| v.short_name.clone().unwrap_or_default());
            (v.short_name.unwrap_or_default(), label)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_speed_control_is_not_inverted() {
        // Ours is a multiplier where bigger is slower; theirs is a percentage
        // where positive is faster. Sharing a sign here would make the slider
        // do the opposite of what it says.
        assert_eq!(percent_change(1.0), 0);
        assert_eq!(percent_change(1.25), -20); // a quarter slower
        assert_eq!(percent_change(0.5), 100); // twice as fast
        assert_eq!(percent_change(0.0), 0); // nonsense in, nothing done
    }

    #[test]
    fn rubbish_is_refused_rather_than_played() {
        // Anything but audio must produce an error, not silence or a crash.
        assert!(decode_mp3(b"not an mp3 at all").is_err());
        assert!(decode_mp3(b"").is_err());
    }

    #[test]
    fn every_language_has_a_voice() {
        for lang in Lang::ALL {
            assert!(!default_voice(lang).is_empty());
        }
    }
}
