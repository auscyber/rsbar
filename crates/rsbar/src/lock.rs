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
    #[error("another rsbar is already running (holding {path})")]
    Held { path: PathBuf },
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
