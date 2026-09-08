//! Running item scripts.
//!
//! Scripts talk back by invoking the CLI, so nothing here needs their stdout —
//! but stderr is captured, because a script that fails silently is the single
//! most annoying thing to debug in a bar config.
//!
//! Two deliberate choices:
//!
//! A script runs as a task on the shared runtime, not the main thread.
//! Anything else lets a slow script stall drawing, and the whole point of the
//! main thread here is that it stays responsive.
//!
//! The exit status is advisory. The daemon links `AppKit`, which may install
//! its own `SIGCHLD` disposition and reap our child before we get to it; the
//! wait then fails with `ECHILD` even though the program ran perfectly. What
//! the pipes produced is the truth, and a lost reap is logged at debug rather
//! than reported as a failure.
//!
//! # `sbar.exec`
//!
//! There is no `Request::Exec` here on purpose. `rsbar-lua`'s `exec_fn`
//! already spawns `sbar.exec` calls directly via `tokio::process::Command`
//! on the Lua side and never reaches this `Dispatcher` — and real
//! `SketchyBar` has no `exec` primitive either, so this is not a gap to
//! close, just a client-side convenience.

use crate::components::ItemHandle;
use rsbar_protocol::Event;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::AsyncReadExt as _;
use tokio::process::Command;
use tokio::sync::Semaphore;
use tokio::sync::mpsc::{self, UnboundedSender};

/// One script to run, with the context its environment describes.
pub struct Job {
    /// Which item this belongs to, both halves of it: the entity to address a
    /// result back to, and the name a script reads. Looking the name up again
    /// afterwards can find a different item — or nothing — if a reload has
    /// been through in between.
    pub item: ItemHandle,
    /// Shared, not copied. One event dispatched to a dozen items used to clone
    /// the script text a dozen times; this is a reference count instead.
    pub script: Arc<str>,
    /// Why this is running, and what it carries. One value rather than a
    /// sender and a payload, because they were never independent.
    ///
    /// Shared for the same reason: an event with a payload is not small, and
    /// every item watching it was being handed its own copy.
    pub event: Arc<Event>,
}

/// A fixed pool of concurrent scripts.
///
/// Bounded on purpose: unbounded concurrency lets a misconfigured one-second
/// script fork without limit until the machine gives up.
pub struct Runner {
    tx: UnboundedSender<Job>,
}

impl Runner {
    /// Starts a dispatcher task that runs up to `workers` scripts at once. It
    /// stops when the runner is dropped, which closes the channel.
    ///
    /// # Panics
    ///
    /// Never in practice: the task keeps its own `Semaphore` handle alive, so
    /// `acquire_owned` can only fail by that semaphore closing, which nothing
    /// here does.
    #[must_use]
    pub fn start(workers: usize) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<Job>();
        let permits = Arc::new(Semaphore::new(workers.max(1)));

        crate::pool::spawn(async move {
            while let Some(job) = rx.recv().await {
                // Acquired before spawning, so the loop itself stops taking
                // work once every slot is busy, rather than piling up tasks
                // parked on the semaphore.
                let permit = Arc::clone(&permits)
                    .acquire_owned()
                    .await
                    .expect("the semaphore is never closed");
                crate::pool::spawn(async move {
                    run(&job).await;
                    drop(permit);
                });
            }
        });

        Self { tx }
    }

    /// Queues a script. Dropping the result is fine; failures are logged.
    pub fn run(&self, job: Job) {
        if self.tx.send(job).is_err() {
            tracing::error!("script runner has stopped; dropping job");
        }
    }
}

/// Characters that mean the script needs a shell to interpret it.
///
/// A script without any of them is an argv we can exec directly, saving a
/// whole process spawn — about half the cost of an event reaching the screen,
/// since the shell only exists to hand straight over to the real command.
const NEEDS_SHELL: &[char] = &[
    '|', '&', ';', '<', '>', '(', ')', '$', '`', '\\', '"', '\'', '*', '?', '[', '#', '~', '=',
];

/// Splits a shell-free script into an argv.
fn direct_argv(script: &str) -> Option<Vec<&str>> {
    if script.contains(NEEDS_SHELL) {
        return None;
    }
    let argv: Vec<&str> = script.split_whitespace().collect();
    (!argv.is_empty()).then_some(argv)
}

/// `PATH` for anything the daemon spawns, with the directory this binary
/// lives in on the front of it.
///
/// A config and its plugin scripts talk back by running `rsbard`, so that
/// name has to resolve. The daemon knows exactly where it is; nothing else
/// reliably does — a development build sits in `target/debug`, an installed
/// one in whatever prefix it was put in, and neither is on a login shell's
/// `PATH` by accident. Prepending our own directory is the whole fix: no
/// generated wrapper, no second binary, just the real `rsbard` findable under
/// its real name.
///
/// Front rather than back, so a stale `rsbard` installed elsewhere cannot
/// shadow the daemon that is actually running.
#[must_use]
pub fn child_path() -> std::ffi::OsString {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let Some(directory) = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(std::path::Path::to_path_buf))
    else {
        return inherited;
    };

    let mut path = std::ffi::OsString::from(directory);
    if !inherited.is_empty() {
        path.push(":");
        path.push(&inherited);
    }
    path
}

async fn run(job: &Job) {
    let mut command = if let Some(argv) = direct_argv(&job.script) {
        let mut command = Command::new(argv[0]);
        command.args(&argv[1..]);
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(job.script.as_ref());
        command
    };

    let mut child = match command
        .env("NAME", job.item.name().as_str())
        .envs(job.event.env())
        .env("RSBAR_SERVICE", rsbar_protocol::service_name())
        .env("PATH", child_path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(item = %job.item, %err, "could not start script");
            return;
        }
    };

    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        // Read to EOF before waiting: a child that fills the pipe buffer
        // blocks forever if nobody drains it.
        let _ = pipe.read_to_string(&mut stderr).await;
    }

    match child.wait().await {
        Ok(status) if !status.success() => {
            tracing::warn!(item = %job.item, ?status, stderr = %stderr.trim(), "script failed");
        }
        Ok(_) => {
            if !stderr.trim().is_empty() {
                tracing::debug!(item = %job.item, stderr = %stderr.trim(), "script wrote to stderr");
            }
        }
        // Someone else reaped the child. The script still ran.
        Err(err) => {
            tracing::debug!(item = %job.item, %err, "could not reap script; it still ran");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::direct_argv;

    #[test]
    fn a_plain_command_needs_no_shell() {
        assert_eq!(
            direct_argv("rsbar item set clock"),
            Some(vec!["rsbar", "item", "set", "clock"])
        );
    }

    #[test]
    fn anything_a_shell_would_interpret_gets_a_shell() {
        // Substitution, pipes, redirection, quoting and globs all change
        // meaning if handed straight to exec.
        for script in [
            "echo $(date)",
            "a | b",
            "a && b",
            "a; b",
            "a > f",
            "echo 'quoted'",
            "echo \"quoted\"",
            "ls *.txt",
            "echo $HOME",
            "FOO=1 cmd",
        ] {
            assert!(
                direct_argv(script).is_none(),
                "{script} must go through a shell"
            );
        }
    }

    #[test]
    fn an_empty_script_is_not_a_command() {
        assert_eq!(direct_argv(""), None);
        assert_eq!(direct_argv("   "), None);
    }
}
