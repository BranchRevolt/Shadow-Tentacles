// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! What the machine can run: VRAM, and system RAM for the CPU-only case.
//!
//! Every probe is best-effort and returns 0 on failure, which callers read as
//! unknown and therefore as the conservative case.

use sysinfo::System;

#[derive(Debug, Clone, Copy)]
pub struct Hardware {
    pub vram_mb: u32,
    pub ram_mb: u32,
    pub cpu_threads: u32,
}

impl Hardware {
    pub fn probe() -> Self {
        let mut sys = System::new();
        sys.refresh_memory();
        Self {
            vram_mb: available_vram_mb(),
            ram_mb: (sys.total_memory() / 1_048_576) as u32,
            cpu_threads: std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(4),
        }
    }

    /// A one-line description for the setup wizard and `shadow doctor`.
    pub fn describe(&self) -> String {
        let gpu = if self.vram_mb == 0 {
            "no usable GPU detected".to_string()
        } else {
            format!("{:.1} GB VRAM", self.vram_mb as f64 / 1024.0)
        };
        format!(
            "{gpu}, {:.1} GB RAM, {} CPU threads",
            self.ram_mb as f64 / 1024.0,
            self.cpu_threads
        )
    }
}

/// Total VRAM in MB for the primary GPU, or 0 if unknown.
///
/// Tried in order: nvidia-smi, /sys/class/drm on Linux AMD and Intel,
/// system_profiler on macOS, wmic on Windows.
pub fn available_vram_mb() -> u32 {
    #[cfg(target_os = "macos")]
    {
        if let Some(v) = macos_vram() {
            return v;
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Some(v) = nvidia_smi_vram() {
            return v;
        }
        if let Some(v) = linux_sysfs_vram() {
            return v;
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Some(v) = nvidia_smi_vram() {
            return v;
        }
        if let Some(v) = wmic_vram() {
            return v;
        }
    }
    0
}

#[cfg(any(target_os = "linux", target_os = "windows"))]
fn nvidia_smi_vram() -> Option<u32> {
    let out = std::process::Command::new("nvidia-smi")
        .args(["--query-gpu=memory.total", "--format=csv,noheader,nounits"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    // One line per GPU; only the first is used.
    s.lines().next().and_then(|l| l.trim().parse::<u32>().ok())
}

#[cfg(target_os = "linux")]
fn linux_sysfs_vram() -> Option<u32> {
    // AMD and Intel expose dedicated VRAM here, in bytes.
    let mut best = 0u64;
    for entry in std::fs::read_dir("/sys/class/drm").ok()?.flatten() {
        let path = entry.path().join("device/mem_info_vram_total");
        if let Ok(s) = std::fs::read_to_string(&path)
            && let Ok(bytes) = s.trim().parse::<u64>()
        {
            best = best.max(bytes);
        }
    }
    (best > 0).then_some((best / 1_048_576) as u32)
}

#[cfg(target_os = "macos")]
fn macos_vram() -> Option<u32> {
    let out = std::process::Command::new("system_profiler")
        .args(["SPDisplaysDataType", "-json"])
        .output()
        .ok()?;
    let json = String::from_utf8_lossy(&out.stdout);
    // Text scan rather than a JSON parse — the shape varies by macOS version.
    for line in json.lines() {
        let line = line.trim();
        if line.starts_with("\"spdisplays_vram\"") {
            let val = line.split(':').nth(1)?.trim().trim_matches('"').trim();
            if let Some(gb) = val.strip_suffix(" GB") {
                return gb.trim().parse::<u32>().ok().map(|g| g * 1024);
            }
            if let Some(mb) = val.strip_suffix(" MB") {
                return mb.trim().parse::<u32>().ok();
            }
        }
    }
    // Apple Silicon has unified memory: total RAM is the right proxy.
    let mut sys = System::new();
    sys.refresh_memory();
    Some((sys.total_memory() / 1_048_576) as u32)
}

#[cfg(target_os = "windows")]
fn wmic_vram() -> Option<u32> {
    let out = std::process::Command::new("wmic")
        .args(["path", "Win32_VideoController", "get", "AdapterRAM"])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    s.lines().filter_map(|l| l.trim().parse::<u64>().ok()).next().map(|b| (b / 1_048_576) as u32)
}
