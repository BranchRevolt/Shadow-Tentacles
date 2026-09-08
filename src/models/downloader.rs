// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Fetching a GGUF to disk.
//!
//! A `.tmp` staging name, a `Range` request to resume an interrupted download,
//! and an optional checksum.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::error::{AppError, AppResult};
use crate::shutdown::Cancel;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Downloading,
    Verifying,
}

#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub phase: Phase,
    /// Bytes a second, or zero before there is enough to say.
    ///
    /// Smoothed over the recent past rather than averaged since the start: a
    /// resumed download begins with gigabytes already on disk.
    pub bytes_per_second: u64,
}

impl Progress {
    pub fn percent(&self) -> u8 {
        if self.total == 0 {
            return 0;
        }
        ((self.downloaded.saturating_mul(100) / self.total).min(100)) as u8
    }
}

/// How fast the bytes are arriving.
///
/// Sampled over a window rather than per chunk, because chunks land in bursts
/// and a per-chunk figure flickers between nothing and everything. Each closed
/// window is blended into the last, so the number moves but does not twitch.
#[derive(Debug)]
struct Rate {
    window_started: Instant,
    window_bytes: u64,
    smoothed: f64,
}

impl Default for Rate {
    fn default() -> Rate {
        Rate { window_started: Instant::now(), window_bytes: 0, smoothed: 0.0 }
    }
}

/// How often progress is reported at the most, when the percentage is not
/// moving.
const TELL_EVERY: Duration = Duration::from_millis(500);

impl Rate {
    /// Long enough to average out a burst, short enough to notice the network
    /// changing its mind.
    const WINDOW: Duration = Duration::from_millis(700);

    fn saw(&mut self, bytes: u64) {
        self.saw_at(bytes, Instant::now());
    }

    /// The clock is a parameter so the smoothing can be tested without waiting
    /// for it.
    fn saw_at(&mut self, bytes: u64, now: Instant) {
        self.window_bytes += bytes;
        let elapsed = now.saturating_duration_since(self.window_started);
        if elapsed < Self::WINDOW {
            return;
        }
        let sample = self.window_bytes as f64 / elapsed.as_secs_f64();
        self.smoothed = match self.smoothed {
            0.0 => sample,
            old => old * 0.6 + sample * 0.4,
        };
        self.window_started = now;
        self.window_bytes = 0;
    }

    /// Zero until the first window closes: better silence than a figure made
    /// up from a tenth of a second.
    fn per_second(&self) -> u64 {
        self.smoothed as u64
    }
}

/// Download `url` into `dest_dir/filename`, resuming a previous attempt if one
/// is staged. Returns the final path.
///
/// If the destination already exists it is returned as-is (verified first when
/// a checksum is known), so calling this on every startup is cheap and safe.
pub async fn download(
    url: &str,
    dest_dir: &Path,
    filename: &str,
    expected_sha256: Option<&str>,
    cancel: &Cancel,
    mut on_progress: impl FnMut(Progress),
) -> AppResult<PathBuf> {
    tokio::fs::create_dir_all(dest_dir).await?;
    let dest = dest_dir.join(filename);
    let tmp = dest.with_extension("gguf.part");

    if dest.exists() {
        match expected_sha256 {
            Some(hash) if !verify_sha256(&dest, hash, cancel, &mut on_progress).await? => {
                tracing::warn!("{} failed verification, re-downloading", dest.display());
                tokio::fs::remove_file(&dest).await?;
            }
            _ => return Ok(dest),
        }
    }

    // Resume from whatever a previous run managed to stage.
    let already = match tokio::fs::metadata(&tmp).await {
        Ok(m) => m.len(),
        Err(_) => 0,
    };

    let client = reqwest::Client::builder()
        .user_agent(crate::USER_AGENT)
        // No overall timeout: this request legitimately runs for many minutes.
        // The read timeout below is what detects a dead connection.
        .read_timeout(std::time::Duration::from_secs(60))
        .build()?;

    let mut request = client.get(url);
    if already > 0 {
        tracing::info!("resuming {filename} at {already} bytes");
        request = request.header(reqwest::header::RANGE, format!("bytes={already}-"));
    }

    let response = request.send().await?;
    let status = response.status();
    if !status.is_success() {
        return Err(AppError::ModelDownload(format!("{url} returned HTTP {status}")));
    }

    // A server that ignores Range replies 200 with the whole file; then the
    // staged bytes are meaningless and the download starts over rather than append.
    let resuming = already > 0 && status == reqwest::StatusCode::PARTIAL_CONTENT;
    let mut downloaded = if resuming { already } else { 0 };
    let total = response.content_length().unwrap_or(0) + downloaded;

    let mut file = if resuming {
        tokio::fs::OpenOptions::new().append(true).open(&tmp).await?
    } else {
        tokio::fs::File::create(&tmp).await?
    };

    let mut stream = response.bytes_stream();
    let mut last_report = 0u8;
    let mut last_told = Instant::now();
    let mut rate = Rate::default();
    on_progress(Progress { downloaded, total, phase: Phase::Downloading, bytes_per_second: 0 });

    while let Some(chunk) = stream.next().await {
        if cancel.is_cancelled() {
            // Leave the .part file: a cancel is usually "not now", and the next
            // attempt resumes instead of re-downloading gigabytes.
            file.flush().await?;
            return Err(AppError::Cancelled);
        }
        let chunk = chunk?;
        file.write_all(&chunk).await?;
        downloaded += chunk.len() as u64;
        rate.saw(chunk.len() as u64);

        let p = Progress {
            downloaded,
            total,
            phase: Phase::Downloading,
            bytes_per_second: rate.per_second(),
        };
        // On the percent, or on the clock. One percent of a 2.4 GB model is
        // 24 MB — a minute of silence on a slow line, and a speed that stood
        // still between updates. The clock bounds it at two a second, which is
        // as often as a person can read a changing number anyway.
        if p.percent() != last_report || last_told.elapsed() >= TELL_EVERY {
            last_report = p.percent();
            last_told = Instant::now();
            on_progress(p);
        }
    }
    file.flush().await?;
    drop(file);

    // Left nested on purpose. Collapsing these into one condition would put a
    // full read of a five-gigabyte file inside an `&&`, where it reads like a
    // cheap test and is not.
    #[allow(clippy::collapsible_if)]
    if let Some(hash) = expected_sha256 {
        if !verify_sha256(&tmp, hash, cancel, &mut on_progress).await? {
            tokio::fs::remove_file(&tmp).await?;
            return Err(AppError::ModelDownload(format!(
                "{filename} failed checksum verification"
            )));
        }
    }

    // Rename last: until this point nothing looks like a usable model.
    tokio::fs::rename(&tmp, &dest).await?;
    tracing::info!("downloaded {}", dest.display());
    Ok(dest)
}

/// Delete `.part` files left behind by an interrupted run older than the model
/// currently on disk. Called at startup so a stale partial never confuses a
/// resume against a different URL.
pub async fn clean_partials(dir: &Path) -> AppResult<()> {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return Ok(());
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("part") {
            // Only remove a partial whose finished file already exists.
            let finished = path.with_extension("");
            if finished.exists() {
                tracing::info!("removing stale partial {}", path.display());
                let _ = tokio::fs::remove_file(&path).await;
            }
        }
    }
    Ok(())
}

async fn verify_sha256(
    path: &Path,
    expected: &str,
    cancel: &Cancel,
    on_progress: &mut impl FnMut(Progress),
) -> AppResult<bool> {
    use tokio::io::AsyncReadExt;

    let total = tokio::fs::metadata(path).await?.len();
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut read_total = 0u64;
    let mut last = 0u8;

    loop {
        if cancel.is_cancelled() {
            return Err(AppError::Cancelled);
        }
        let n = file.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        read_total += n as u64;

        // No rate for the check: it reads the disk, and a number that looked
        // like a download speed but was not would be worse than none.
        let p = Progress {
            downloaded: read_total,
            total,
            phase: Phase::Verifying,
            bytes_per_second: 0,
        };
        if p.percent() != last {
            last = p.percent();
            on_progress(p);
        }
    }

    let actual = hex::encode(hasher.finalize());
    Ok(actual.eq_ignore_ascii_case(expected.trim()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A meter whose window opened at a known moment, so the arithmetic in
    /// these tests is exact rather than nearly.
    fn meter_at(start: Instant) -> Rate {
        Rate { window_started: start, ..Rate::default() }
    }

    #[test]
    fn there_is_no_speed_until_there_is_something_to_measure() {
        let start = Instant::now();
        let mut rate = meter_at(start);
        // A tenth of a second of bytes says nothing about a download that will
        // take ten minutes, so nothing is what it reports.
        rate.saw_at(1_000_000, start + Duration::from_millis(100));
        assert_eq!(rate.per_second(), 0);
    }

    #[test]
    fn a_closed_window_gives_the_rate_over_that_window() {
        let start = Instant::now();
        let mut rate = meter_at(start);
        rate.saw_at(7_000_000, start + Duration::from_millis(1000));
        assert_eq!(
            rate.per_second(),
            7_000_000,
            "семь мегабайт за секунду — семь мегабайт в секунду"
        );
    }

    #[test]
    fn the_rate_follows_a_change_without_jumping_to_it() {
        let mut now = Instant::now();
        let mut rate = meter_at(now);

        now += Duration::from_millis(1000);
        rate.saw_at(10_000_000, now);
        assert_eq!(rate.per_second(), 10_000_000);

        // The line drops to a tenth. The reading has to move a long way, but
        // not all the way at once, or a single stalled second reads as a dead
        // connection.
        now += Duration::from_millis(1000);
        rate.saw_at(1_000_000, now);
        let after_one = rate.per_second();
        assert!(after_one < 10_000_000 && after_one > 1_000_000, "{after_one}");

        now += Duration::from_millis(1000);
        rate.saw_at(1_000_000, now);
        assert!(rate.per_second() < after_one, "и продолжает идти к новой скорости");
    }
}
