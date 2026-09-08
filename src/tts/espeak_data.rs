// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Putting espeak-ng's phoneme data where the library will find it.
//!
//! `build.rs` packs the data into the binary, and it is written to the data
//! directory on first use; the path compiled into the library is the build
//! machine's.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::paths;

/// Every file of `espeak-ng-data`, packed by `build.rs`.
const PACKED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/espeak-ng-data.pack"));

/// The directory name espeak-rs looks for inside the one it is given.
const DIR: &str = "espeak-ng-data";

/// The variable espeak-rs reads before it tries anywhere else.
const VARIABLE: &str = "PIPER_ESPEAKNG_DATA_DIRECTORY";

/// Written beside the data so a changed pack replaces an unpacked one.
const STAMP: &str = ".pack-len";

static READY: OnceLock<()> = OnceLock::new();

/// Unpack the data if it is not there, and point espeak-ng at it.
///
/// Called before every phonemization; the work happens once per run and, on
/// disk, once per installation.
pub fn ensure() {
    READY.get_or_init(|| {
        match unpack() {
            Ok(parent) => {
                // SAFETY: set before any thread has called into espeak-ng,
                // which is what `READY` and the lock in `phonemes` guarantee.
                unsafe { std::env::set_var(VARIABLE, &parent) };
            }
            Err(why) => {
                tracing::error!("could not unpack the speech data: {why}; reading aloud will fail");
            }
        }
    });
}

/// Write the data to the data directory, and return the directory holding it.
fn unpack() -> Result<PathBuf, String> {
    let parent = paths::data_dir().map_err(|e| e.to_string())?;
    let data = parent.join(DIR);
    if is_current(&data) {
        return Ok(parent);
    }

    let staging = parent.join(format!("{DIR}.unpacking"));
    let _ = fs::remove_dir_all(&staging);
    fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;

    for (name, body) in files()? {
        let path = staging.join(&name);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fs::write(&path, body).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    fs::write(staging.join(STAMP), PACKED.len().to_string())
        .map_err(|e| format!("{}: {e}", staging.display()))?;

    let _ = fs::remove_dir_all(&data);
    fs::rename(&staging, &data).map_err(|e| format!("{}: {e}", data.display()))?;
    tracing::info!("unpacked the speech data to {}", data.display());
    Ok(parent)
}

/// Whether `data` already holds this build's pack.
fn is_current(data: &Path) -> bool {
    fs::read_to_string(data.join(STAMP))
        .map(|stamp| stamp.trim() == PACKED.len().to_string())
        .unwrap_or(false)
}

/// Walk the pack, yielding each file's path and contents.
fn files() -> Result<Vec<(String, &'static [u8])>, String> {
    let mut at = 0usize;
    let count = take_u32(&mut at)? as usize;
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        let name_len = take_u32(&mut at)? as usize;
        let name = take(&mut at, name_len)?;
        let name = std::str::from_utf8(name).map_err(|e| e.to_string())?;
        if name.contains("..") || name.starts_with('/') {
            return Err(format!("the pack names a file outside its directory: {name}"));
        }
        let body_len = take_u32(&mut at)? as usize;
        out.push((name.to_string(), take(&mut at, body_len)?));
    }
    Ok(out)
}

fn take(at: &mut usize, len: usize) -> Result<&'static [u8], String> {
    let end = at.checked_add(len).ok_or("the packed speech data is truncated")?;
    let slice = PACKED.get(*at..end).ok_or("the packed speech data is truncated")?;
    *at = end;
    Ok(slice)
}

fn take_u32(at: &mut usize) -> Result<u32, String> {
    let bytes = take(at, 4)?;
    Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}
