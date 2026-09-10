//! One daemon at a time.
//!
//! The service name would catch a second copy on its own — binding it fails
//! when it is taken — but the failure reads as a name problem rather than
//! "one is already running", and it happens after the bar's windows exist.
//! Taking a lock first says what is actually wrong, before anything is drawn.
//!
//! `fcntl` rather than a pid file, following `SketchyBar`: the kernel drops
//! the lock when the process dies, including on a signal it cannot handle, so
//! a leftover file never blocks a restart. A pid file has to be cleaned up by
//! the very process that just crashed.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Held for as long as the daemon runs. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    _file: File,
    path: PathBuf,
}

impl Lock {
    /// The lock's path, for a log line worth reading.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("could not open the lock file at {path}: {source}")]
    Open {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("another coolabah is already running (holding {path})")]
    Held { path: PathBuf },
}

/// Proof of the main thread **and** of being the only daemon.
///
/// Two facts that always travel together and were never checked together. The
/// main thread is where the window server and Carbon must be spoken to; the
/// lock is what says this process is the one allowed to. Creating the bar's
/// windows or claiming the service name needs both, and until now both were
/// established in `main` and then forgotten — nothing downstream could tell a
/// second daemon apart from the first.
///
/// Holding this *is* holding the lock: dropping it releases, so the claim
/// lasts exactly as long as something can still act on it.
///
/// `Deref` twice over, so it is accepted anywhere either fact is asked for —
/// `&*sole` where a [`MainThread`](skylight::MainThread) is wanted, `**sole`
/// for the `MainThreadMarker` a `#[skylight::main_thread]` function takes.
#[derive(Debug)]
pub struct Sole {
    main: skylight::MainThread,
    /// Held, not merely witnessed.
    lock: Lock,
}

impl Sole {
    /// The lock's path, for a log line worth reading.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        self.lock.path()
    }
}

impl std::ops::Deref for Sole {
    type Target = skylight::MainThread;

    fn deref(&self) -> &skylight::MainThread {
        &self.main
    }
}

/// Whether this process already handed one out.
///
/// The file lock alone does not answer it: a `fcntl` record lock belongs to
/// the *process*, so a second `F_SETLK` on a file this process already locked
/// succeeds. That is right for what the lock is for — keeping a second daemon
/// out — and it means the lock cannot also promise there is one [`Sole`] here.
/// This does.
static TAKEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Takes the lock and pairs it with the proof, or reports who has it.
///
/// # Errors
///
/// [`Error::Held`] if another process holds the lock, or if this process
/// already has a [`Sole`] alive. [`Error::Open`] if the file cannot be created.
#[skylight::main_thread]
pub fn only(service: &str) -> Result<Sole, Error> {
    let lock = acquire(service)?;
    if TAKEN.swap(true, std::sync::atomic::Ordering::AcqRel) {
        return Err(Error::Held {
            path: lock.path().to_path_buf(),
        });
    }
    Ok(Sole {
        main: skylight::MainThread::of(&proof),
        lock,
    })
}

impl Drop for Sole {
    fn drop(&mut self) {
        TAKEN.store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Takes the lock for this user, or reports who has it.
///
/// Per user rather than system-wide: two people logged in at once each get
/// their own bar, and one should not lock the other out.
///
/// # Errors
///
/// [`Error::Held`] if another daemon has it, [`Error::Open`] if the file
/// cannot be created at all.
pub fn acquire(service: &str) -> Result<Lock, Error> {
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_owned());
    let path = PathBuf::from(format!("/tmp/{service}_{user}.lock"));

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)
        .map_err(|source| Error::Open {
            path: path.clone(),
            source,
        })?;

    let lock = libc::flock {
        l_start: 0,
        l_len: 0,
        l_pid: std::process::id().cast_signed(),
        l_type: libc::F_WRLCK,
        //  is 0; the field is a short, and the cast is exact.
        l_whence: i16::try_from(libc::SEEK_SET).unwrap_or(0),
    };
    // SAFETY: `file` is open for writing and outlives the call; `lock`
    // describes the whole file.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETLK, &raw const lock) } == -1 {
        return Err(Error::Held { path });
    }

    Ok(Lock { _file: file, path })
}

#[cfg(test)]
mod tests {
    use super::only;

    /// The point of the type: one value that answers both questions, and
    /// releases the lock when it goes.
    #[test]
    fn the_token_proves_the_main_thread_and_holds_the_lock() {
        let service = format!("coolabah-lock-test-{}", std::process::id());
        let sole = only(crate::runloop::main_thread(), &service).expect("the lock was free");

        // Accepted where a bare marker is wanted, and where the fuller proof
        // is -- both by `Deref`, so no call site has to unpack it.
        let _: objc2::MainThreadMarker = **sole;
        let _ = skylight::MainThreadProof::run_loop(&*sole);

        // A second daemon is refused while this one holds it.
        assert!(
            matches!(
                only(crate::runloop::main_thread(), &service),
                Err(super::Error::Held { .. })
            ),
            "a second token was handed out while the first was alive"
        );

        // And released when it goes, so a restart is not blocked.
        drop(sole);
        assert!(
            only(crate::runloop::main_thread(), &service).is_ok(),
            "the lock outlived the token holding it"
        );
    }
}
