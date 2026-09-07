//! The config file, and re-running it when it changes.
//!
//! Config is a shell script that drives the CLI — the same shape `SketchyBar`
//! uses, and the reason there is no config *format* to parse. Reloading is
//! therefore re-running it, not re-reading it.
//!
//! The current state lives behind an `RwLock` because two threads need it: the
//! config thread writes when a run finishes, and the app reads every tick to
//! decide whether that run has settled. Readers dominate, which is what an
//! `RwLock` is for over a `Mutex`.
//!
//! `tokio::sync`'s lock rather than `std`'s. Its `blocking_read`/`blocking_write`
//! panic if called from inside an async runtime — this daemon deliberately has
//! none, its executor is the `CFRunLoop`-driven Bevy runner, so they are the
//! correct call here and not the trap they look like.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Shared between the watcher thread and the app.
pub type Shared = Arc<RwLock<Config>>;

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// Where the config is, if there is one. A daemon with no config file is
    /// legitimate: everything can be driven over the CLI instead.
    pub path: Option<PathBuf>,
    /// Bumped on every successful run, so a reader can tell whether what it
    /// last saw is still current.
    pub generation: u64,
    /// Whether a run is in flight. The app watches this to know when to
    /// settle a reload.
    pub running: bool,
    /// What went wrong on the last attempt, if anything. Kept rather than
    /// logged and dropped, so `--query` can surface a broken config instead of
    /// leaving the user staring at a bar that quietly stopped updating.
    pub last_error: Option<String>,
}

/// Resolves the config path, honouring `RSBAR_CONFIG` first so a development
/// build can run against its own.
#[must_use]
pub fn discover() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("RSBAR_CONFIG") {
        let path = PathBuf::from(path);
        return path.exists().then_some(path);
    }

    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?;

    let path = base.join("rsbar").join("rsbarrc");
    path.exists().then_some(path)
}

#[must_use]
pub fn shared() -> Shared {
    Arc::new(RwLock::new(Config {
        path: discover(),
        generation: 0,
        running: false,
        last_error: None,
    }))
}

/// Runs the config script to completion.
///
/// Synchronous on purpose. A config is a burst of hundreds of `set` calls and
/// the bar should not paint half of one, so the caller waits and repaints once
/// at the end.
///
/// # Errors
///
/// Returns a message suitable for showing the user if the script cannot be run
/// or exits non-zero.
pub fn run(path: &Path, service: &str) -> Result<(), String> {
    let directory = path.parent().unwrap_or(Path::new("."));

    let output = std::process::Command::new("/bin/sh")
        .arg(path)
        .current_dir(directory)
        .env("RSBAR_SERVICE", service)
        // Config scripts reference their own directory for plugins and assets,
        // so give them a name for it rather than making them derive it.
        .env("RSBAR_CONFIG_DIR", directory)
        .output()
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
