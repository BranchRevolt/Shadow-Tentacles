// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Linux and WebKitGTK: forcing a relayout after the window changes monitor.
//!
//! Goes through GTK rather than `WebviewWindow::set_size`, because Tauri's
//! `inner_size()` includes the frame under client-side decorations while
//! `set_size` does not, and mixing the two grows the window on every move.
//!
//! A no-op on Windows and macOS.

#[cfg(target_os = "linux")]
mod imp {
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use gtk::glib;
    use gtk::prelude::GtkWindowExt;
    use tauri::{AppHandle, Manager, WebviewWindow, WindowEvent};

    /// Where geometry is traced, in debug builds only.
    static TRACE_PATH: OnceLock<PathBuf> = OnceLock::new();
    static TRACE_LINES: AtomicUsize = AtomicUsize::new(0);
    /// A ceiling, so a long session does not fill the disk with window moves.
    const TRACE_MAX_LINES: usize = 2_000;

    /// How long GTK gets to apply the one-pixel step before it is taken back.
    const NUDGE_STEP: Duration = Duration::from_millis(60);
    /// Quiet time after the step before another relayout may be triggered.
    const NUDGE_SETTLE: Duration = Duration::from_millis(120);

    fn trace(line: &str) {
        let Some(path) = TRACE_PATH.get() else { return };
        if TRACE_LINES.fetch_add(1, Ordering::Relaxed) >= TRACE_MAX_LINES {
            return;
        }
        tracing::debug!("window {line}");
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// What the window and its monitor say about themselves.
    ///
    /// Read these numbers knowing that tao scales every size by the monitor's
    /// factor: an 1180-wide window reads as 2360 here, and a 1080p screen at
    /// 1.25x as 3072 wide.
    fn log_geometry(window: &WebviewWindow, event: &str) {
        if TRACE_PATH.get().is_none() {
            return;
        }
        let monitor = window.current_monitor().ok().flatten();
        trace(&format!(
            "{event}: inner={:?} win_scale={:?} max={:?} monitor={:?} mon_size={:?} mon_scale={:?}",
            window.inner_size().ok().map(|s| (s.width, s.height)),
            window.scale_factor().ok(),
            window.is_maximized().ok(),
            monitor.as_ref().and_then(|m| m.name().cloned()),
            monitor.as_ref().map(|m| (m.size().width, m.size().height)),
            monitor.as_ref().map(|m| m.scale_factor()),
        ));
    }

    /// Which screen the window is on. The scale is part of the identity: two
    /// monitors may differ in nothing else, and one can be rescaled in place.
    fn monitor_key(window: &WebviewWindow) -> Option<String> {
        let monitor = window.current_monitor().ok().flatten()?;
        let name = monitor.name().cloned().unwrap_or_default();
        // A float, so formatted rather than compared bit for bit.
        Some(format!("{name}@{:.4}", monitor.scale_factor()))
    }

    /// Make WebKitGTK lay itself out again, by resizing one pixel and back.
    fn force_relayout(window: &WebviewWindow, busy: Arc<AtomicBool>) {
        // The resize comes back through this same handler as Moved and Resized;
        // the flag is what stops it feeding itself.
        if busy.swap(true, Ordering::SeqCst) {
            return;
        }
        if window.is_maximized().unwrap_or(false) || window.is_fullscreen().unwrap_or(false) {
            busy.store(false, Ordering::SeqCst);
            return;
        }

        let handle = window.clone();
        let done = busy.clone();
        // GTK objects belong to the main thread, so the closure runs there.
        let sent = window.run_on_main_thread(move || {
            let Ok(gtk_window) = handle.gtk_window() else {
                done.store(false, Ordering::SeqCst);
                return;
            };
            let (width, height) = gtk_window.size();
            trace(&format!("relayout: gtk=({width}, {height})"));
            gtk_window.resize(width, height + 1);
            glib::timeout_add_local_once(NUDGE_STEP, move || {
                gtk_window.resize(width, height);
                // Let the resize settle before real moves are listened for again.
                glib::timeout_add_local_once(NUDGE_SETTLE, move || {
                    done.store(false, Ordering::SeqCst);
                });
            });
        });
        if sent.is_err() {
            busy.store(false, Ordering::SeqCst);
        }
    }

    /// Shrink the window if it opens larger than the screen can show.
    ///
    /// The configured size fits a 1080p screen at 1x; at a higher scale factor
    /// the same panel offers fewer logical pixels. Runs once, at startup.
    fn fit_to_work_area(window: &WebviewWindow) {
        let Some(monitor) = window.current_monitor().ok().flatten() else {
            return;
        };
        let scale = monitor.scale_factor();
        if scale <= 0.0 {
            return;
        }
        // Tauri reports the work area in physical pixels; GTK sizes windows in
        // logical ones.
        let area = monitor.work_area().size;
        let max_w = (f64::from(area.width) / scale) as i32;
        let max_h = (f64::from(area.height) / scale) as i32;

        let handle = window.clone();
        let _ = window.run_on_main_thread(move || {
            let Ok(gtk_window) = handle.gtk_window() else {
                return;
            };
            let (width, height) = gtk_window.size();
            let (fit_w, fit_h) = (width.min(max_w), height.min(max_h));
            trace(&format!(
                "fit: gtk=({width}, {height}) work=({max_w}, {max_h}) -> ({fit_w}, {fit_h})"
            ));
            if (fit_w, fit_h) != (width, height) {
                gtk_window.resize(fit_w, fit_h);
            }
        });
    }

    pub fn install(app: &AppHandle) {
        let Some(window) = app.get_webview_window("main") else {
            return;
        };

        // Debug builds only, and emptied on each run.
        if cfg!(debug_assertions)
            && let Ok(dir) = crate::paths::data_dir()
        {
            let path = dir.join("window-debug.log");
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, "");
            let _ = TRACE_PATH.set(path);
        }
        log_geometry(&window, "start");
        fit_to_work_area(&window);

        let watched = window.clone();
        let last_key = Mutex::new(monitor_key(&watched));
        let busy = Arc::new(AtomicBool::new(false));
        // `Moved` arrives continuously while a window is dragged, so the monitor
        // lookup behind it is throttled.
        let last_check = Mutex::new(Instant::now() - Duration::from_secs(1));

        window.on_window_event(move |event| {
            match event {
                WindowEvent::Moved(_) => log_geometry(&watched, "moved"),
                // Under Wayland a move to another output arrives as a
                // reconfigure. Recorded, not acted on: acting on it is how the
                // window grew by the frame on every move.
                WindowEvent::Resized(_) => {
                    log_geometry(&watched, "resized");
                    return;
                }
                WindowEvent::ScaleFactorChanged { .. } => log_geometry(&watched, "rescaled"),
                _ => return,
            }

            {
                let mut last = match last_check.lock() {
                    Ok(l) => l,
                    Err(_) => return,
                };
                if last.elapsed() < Duration::from_millis(100) {
                    return;
                }
                *last = Instant::now();
            }

            let key = monitor_key(&watched);
            // A window halfway between two screens reports either of them; `busy`
            // absorbs the flapping until the move settles.
            let changed = match last_key.lock() {
                Ok(mut last) => {
                    let changed = key.is_some() && *last != key;
                    if changed {
                        *last = key;
                    }
                    changed
                }
                Err(_) => false,
            };

            if changed {
                force_relayout(&watched, busy.clone());
            }
        });
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    pub fn install(_app: &tauri::AppHandle) {}
}

pub use imp::install;
