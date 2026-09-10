//! The config file, and re-running it when it changes.
//!
//! Config is a shell script that drives the CLI — the same shape `SketchyBar`
//! uses, and the reason there is no config *format* to parse. Reloading is
//! therefore re-running it, not re-reading it.
//!
//! The current state lives behind an `RwLock` because more than one place
//! needs it: a reload task writes when a run finishes, and the watcher reads
//! the path. Readers dominate, which is what an `RwLock` is for over a
//! `Mutex`.
//!
//! **The compositing thread does not take this lock while the daemon is
//! running.** It used to, to poll a `running` flag; the flag is gone and the
//! result now comes back on a channel, awaited as a task. That matters
//! because `tokio::sync::RwLock` queues writers fairly: a read taken on the
//! run loop thread while a reload task holds the write guard parks the run
//! loop in the kernel. The one remaining `blocking_read` on that thread is
//! [`crate::ecs::build`]'s, before the loop is entered and before any reload
//! task exists.
//!
//! `tokio::sync`'s lock rather than `std`'s: its `blocking_*` calls are the
//! right ones from a plain worker thread, and the daemon's own executor is a
//! hand-polled `tokio::task::LocalSet` on the run loop rather than a full
//! tokio runtime, so they do not panic.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Shared between the watcher thread and the app.
pub type Shared = Arc<RwLock<Config>>;

/// What one config run came to: nothing, or the reason it did not happen.
///
/// `Err` deliberately covers "there is no config file" as well as "the script
/// failed". Both mean the same thing to whoever asked for the reload —
/// nothing was replaced — and the items the previous config put on the bar
/// must stay.
pub type Outcome = Result<(), String>;

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// A daemon with no config file is legitimate: everything can be driven
    /// over the CLI instead.
    pub path: Option<PathBuf>,
    /// Bumped on every successful run, so a reader can tell whether what it
    /// last saw is still current.
    pub generation: u64,
    /// What went wrong on the last attempt, if anything. Kept rather than
    /// logged and dropped, so `--query` can surface a broken config instead of
    /// leaving the user staring at a bar that quietly stopped updating.
    pub last_error: Option<String>,
}

/// Resolves the config path, honouring `COOLABAH_CONFIG` first so a development
/// build can run against its own.
#[must_use]
pub fn discover() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(name!(env "CONFIG")) {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }

    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    let path = base.join(name!()).join(name!(rc));
    path.exists().then_some(path)
}

#[must_use]
pub fn shared() -> Shared {
    Arc::new(RwLock::new(Config {
        path: discover(),
        generation: 0,
        last_error: None,
    }))
}

/// Runs the config script to completion.
///
/// Awaited to completion on purpose. A config is a burst of hundreds of `set`
/// calls and the bar should not paint half of one, so the caller waits and
/// repaints once at the end.
///
/// # Errors
///
/// Returns a message suitable for showing the user if the script cannot be run
/// or exits non-zero.
pub async fn run(path: &Path, service: &str) -> Result<(), String> {
    let directory = path.parent().unwrap_or(Path::new("."));

    // Run it, rather than reading it as shell. A config carries a shebang and
    // means it: `SketchyBar`'s own is `#!/bin/sh`, but an `SbarLua` one is
    // `#!/usr/bin/env lua` and is not shell at all -- handing that to `/bin/sh`
    // is a syntax error on its first line. Executing the file lets the kernel
    // honour the shebang, which is what `SketchyBar` itself does.
    //
    // A config that is not executable falls back to `/bin/sh`, since that is
    // the likeliest intent and a missing `chmod +x` is a poor reason to come
    // up with an empty bar.
    let executable = std::fs::metadata(path).is_ok_and(|meta| {
        std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0
    });
    let mut command = if executable {
        tokio::process::Command::new(path)
    } else {
        let mut sh = tokio::process::Command::new("/bin/sh");
        sh.arg(path);
        sh
    };

    let output = command
        .current_dir(directory)
        .env(name!(env "SERVICE"), service)
        // Config scripts reference their own directory for plugins and assets,
        // so give them a name for it rather than making them derive it.
        .env(name!(env "CONFIG_DIR"), directory)
        // A config talks back by running `coolabah`, so that name has to
        // resolve from inside it. See `crate::script::child_path`.
        .env("PATH", crate::script::child_path())
        .output()
        .await
        .map_err(|err| format!("could not run {}: {err}", path.display()))?;

    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    Err(format!(
        "{} exited with {}: {}",
        path.display(),
        output.status,
        stderr.trim()
    ))
}

/// Coalesces the flurry of filesystem events one save produces.
///
/// Editors write, rename and touch in quick succession, and `FSEvents` reports
/// each. Without this a single save re-runs the config several times.
pub struct Debounce {
    last: Option<Instant>,
    window: Duration,
}

impl Debounce {
    #[must_use]
    pub fn new(window: Duration) -> Self {
        Self { last: None, window }
    }

    /// Whether enough time has passed to treat this as a new change.
    pub fn admit(&mut self) -> bool {
        let now = Instant::now();
        if self
            .last
            .is_some_and(|last| now.duration_since(last) < self.window)
        {
            return false;
        }
        self.last = Some(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_collapses_a_burst() {
        let mut debounce = Debounce::new(Duration::from_millis(50));
        assert!(debounce.admit(), "the first event is always admitted");
        assert!(!debounce.admit(), "a second event in the window is not");
        std::thread::sleep(Duration::from_millis(60));
        assert!(debounce.admit(), "one after the window is");
    }
}
