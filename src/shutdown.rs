// SPDX-FileCopyrightText: 2026 WarpCoreDev
// SPDX-License-Identifier: GPL-3.0-or-later
//! Shutdown coordination.
//!
//! Long-running loops poll one shared flag, and blocking work holds a
//! `WorkerGuard` so exit can wait for it rather than guess. Never
//! `process::exit` while the engine is loaded: that skips the `Drop` that frees
//! VRAM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use parking_lot::{Condvar, Mutex};

/// How long a graceful shutdown may take before the watchdog stops being polite.
pub const GRACE_PERIOD: Duration = Duration::from_secs(10);

pub struct Shutdown {
    requested: AtomicBool,
    /// Number of `WorkerGuard`s alive. Guarded by the mutex the condvar pairs with.
    workers: Mutex<usize>,
    all_done: Condvar,
}

static GLOBAL: OnceLock<Arc<Shutdown>> = OnceLock::new();

impl Shutdown {
    /// The process-wide instance. Everything shares this one.
    pub fn global() -> &'static Arc<Shutdown> {
        GLOBAL.get_or_init(|| {
            Arc::new(Shutdown {
                requested: AtomicBool::new(false),
                workers: Mutex::new(0),
                all_done: Condvar::new(),
            })
        })
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Relaxed)
    }

    /// Ask everything to stop. Idempotent — a second Ctrl+C hits the signal
    /// handler's hard-exit path instead of coming through here again.
    pub fn request(&self) {
        if !self.requested.swap(true, Ordering::SeqCst) {
            tracing::info!("shutdown requested");
        }
    }

    /// Register a unit of blocking work. Hold the guard for as long as the work
    /// runs; dropping it (including while unwinding from an error or a panic)
    /// is what lets `wait_for_workers` return.
    pub fn worker(self: &Arc<Self>) -> WorkerGuard {
        *self.workers.lock() += 1;
        WorkerGuard { owner: self.clone() }
    }

    /// Block until every `WorkerGuard` is gone, or the deadline passes.
    /// Returns true if the workers actually finished.
    pub fn wait_for_workers(&self, timeout: Duration) -> bool {
        let mut n = self.workers.lock();
        if *n == 0 {
            return true;
        }
        tracing::info!("waiting for {} worker(s) to stop", *n);
        let result = self.all_done.wait_for(&mut n, timeout);
        if result.timed_out() {
            tracing::warn!("{} worker(s) still running after {:?}", *n, timeout);
            false
        } else {
            true
        }
    }
}

/// RAII marker for in-flight blocking work. See [`Shutdown::worker`].
pub struct WorkerGuard {
    owner: Arc<Shutdown>,
}

impl Drop for WorkerGuard {
    fn drop(&mut self) {
        let mut n = self.owner.workers.lock();
        *n = n.saturating_sub(1);
        if *n == 0 {
            self.owner.all_done.notify_all();
        }
    }
}

/// A cancellation signal for one job, which also honors the global shutdown.
///
/// Long loops call `is_cancelled()` once per iteration, so stopping one job and
/// quitting the program are the same check at the call site.
#[derive(Clone)]
pub struct Cancel {
    job: Arc<AtomicBool>,
    global: Arc<Shutdown>,
}

impl Cancel {
    pub fn new() -> Self {
        Self { job: Arc::new(AtomicBool::new(false)), global: Shutdown::global().clone() }
    }

    /// A cancel token that only ever fires on global shutdown.
    pub fn global_only() -> Self {
        Self::new()
    }

    pub fn is_cancelled(&self) -> bool {
        self.job.load(Ordering::Relaxed) || self.global.is_requested()
    }

    /// Cancel just this job; the process keeps running.
    pub fn cancel(&self) {
        self.job.store(true, Ordering::Relaxed);
    }

    /// Reset for reuse. Has no effect on a global shutdown, which is one-way.
    pub fn reset(&self) {
        self.job.store(false, Ordering::Relaxed);
    }
}

impl Default for Cancel {
    fn default() -> Self {
        Self::new()
    }
}

/// Install SIGINT, SIGTERM and SIGHUP handling.
///
/// Requires ctrlc's `termination` feature, without which only SIGINT is caught.
/// The first signal asks for a graceful stop; a second exits immediately.
pub fn install_signal_handlers() -> anyhow::Result<()> {
    let hits = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler(move || {
        if hits.swap(true, Ordering::SeqCst) {
            eprintln!("\nforced exit");
            std::process::exit(130);
        }
        eprintln!("\nstopping — finishing the current step, press Ctrl+C again to force");
        Shutdown::global().request();
        // Armed here rather than in teardown: the deadline starts when the
        // user asks to quit, and a model load in between cannot be interrupted.
        // A SIGTERM during a cold load exited after 12.3 s against a promised
        // 10 when nothing counted until the load returned.
        arm_watchdog(GRACE_PERIOD);
    })?;
    Ok(())
}

/// Start the watchdog that guarantees termination.
///
/// Arming it more than once is harmless. The one place that calls
/// `process::exit` with the model possibly still loaded.
pub fn arm_watchdog(deadline: Duration) {
    std::thread::Builder::new()
        .name("shutdown-watchdog".into())
        .spawn(move || {
            std::thread::sleep(deadline);
            eprintln!("shutdown exceeded {deadline:?} — forcing exit");
            std::process::exit(2);
        })
        .ok();
}
