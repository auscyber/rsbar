//! Running item scripts.
//!
//! Scripts talk back by invoking the CLI, so nothing here needs their stdout —
//! but stderr is captured, because a script that fails silently is the single
//! most annoying thing to debug in a bar config.
//!
//! Two deliberate choices:
//!
//! A script runs on a worker thread, not the main one. Anything else lets a
//! slow script stall drawing, and the whole point of the main thread here is
//! that it stays responsive.
//!
//! The exit status is advisory. The daemon links `AppKit`, which may install
//! its own `SIGCHLD` disposition and reap our child before we get to it; the
//! wait then fails with `ECHILD` even though the program ran perfectly. What
//! the pipes produced is the truth, and a lost reap is logged at debug rather
//! than reported as a failure.

use rsbar_protocol::{Event, Info, ItemName};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

/// One script to run, with the context its environment describes.
pub struct Job {
    pub item: ItemName,
    pub script: String,
    pub sender: Event,
    pub info: Info,
}

/// A fixed pool of workers.
///
/// Bounded on purpose: an unbounded pool lets a misconfigured one-second
/// script fork without limit until the machine gives up.
pub struct Runner {
    tx: Sender<Job>,
}

impl Runner {
    /// Starts `workers` threads. They exit when the runner is dropped.
    ///
    /// # Panics
    ///
    /// Panics if a worker thread cannot be spawned, which means the process is
    /// already out of resources.
    #[must_use]
    pub fn start(workers: usize) -> Self {
        let (tx, rx) = channel::<Job>();
        let rx = Arc::new(Mutex::new(rx));

        for index in 0..workers.max(1) {
            let rx: Arc<Mutex<Receiver<Job>>> = Arc::clone(&rx);
            std::thread::Builder::new()
                .name(format!("rsbar-script-{index}"))
                .spawn(move || {
                    loop {
                        // The lock is released before running, so one long
                        // script does not block the others from taking work.
                        let job = {
                            let Ok(guard) = rx.lock() else { break };
                            guard.recv()
                        };
                        match job {
                            Ok(job) => run(&job),
                            Err(_) => break,
                        }
                    }
                })
                .expect("failed to spawn a script worker");
        }

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

fn run(job: &Job) {
    let mut command = if let Some(argv) = direct_argv(&job.script) {
        let mut command = Command::new(argv[0]);
        command.args(&argv[1..]);
        command
    } else {
        let mut command = Command::new("/bin/sh");
        command.arg("-c").arg(&job.script);
        command
    };

    let mut child = match command
        .env("RSBAR_NAME", job.item.as_str())
        .env("RSBAR_SENDER", job.sender.name())
        .envs(job.info.env())
        .env("RSBAR_SERVICE", rsbar_protocol::service_name())
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
        let _ = pipe.read_to_string(&mut stderr);
    }

    match child.wait() {
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
