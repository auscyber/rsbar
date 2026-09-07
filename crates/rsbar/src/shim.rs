//! Making this daemon answer to `sketchybar` as well as to its own name.
//!
//! A `SketchyBar` config does not only talk to the bar through the Lua module
//! it imported. Its plugin scripts shell out to the `sketchybar` binary to set
//! the item they were run for -- `sketchybar --set "$NAME" label="$VOLUME%"`
//! in the user's own `plugins/volume.sh` -- and its `init.lua` finishes by
//! running `sketchybar --update`. Those go by name, through `PATH`, so under
//! `rsbar` they reach the real `SketchyBar` (which is not running) and quietly
//! do nothing. Every script-driven label stays empty.
//!
//! So the daemon puts a directory of its own at the front of `PATH` for the
//! scripts it runs, holding a `sketchybar` symlink back to this executable.
//! The CLI is the same binary, and its grammar is deliberately `SketchyBar`'s,
//! so the call lands where the config meant it to.
//!
//! Keyed by uid rather than by pid: a script that outlives a reload, or a
//! second look at the same path, should find the same link rather than a
//! stale one, and two users on one machine should not share it.

use std::path::{Path, PathBuf};

/// The name a config expects to find on `PATH`.
const NAME: &str = "sketchybar";

/// Creates the shim directory and returns it, or nothing if it could not be
/// made -- in which case scripts simply run with the `PATH` they had.
#[must_use]
pub fn directory() -> Option<PathBuf> {
    let dir = rsbar_protocol::shim_dir();
    link_in(&dir).map(|()| dir)
}

/// Points `<dir>/sketchybar` at this executable.
fn link_in(dir: &Path) -> Option<()> {
    std::fs::create_dir_all(dir).ok()?;
    let link = dir.join(NAME);
    let exe = std::env::current_exe().ok()?;
    if link.read_link().is_ok_and(|target| target == exe) {
        return Some(());
    }
    // Replaced rather than left alone: a development build and an installed
    // one have different paths, and the link has to name the one running now.
    let _ = std::fs::remove_file(&link);
    std::os::unix::fs::symlink(&exe, &link).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of this test's own. The real one is shared by every
    /// process on the machine, so two tests pointed at it race each other
    /// over the same link -- which is a flaw in the test, not in the shim.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rsbar-shim-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn the_shim_points_at_this_executable() {
        let dir = scratch("points-at");
        link_in(&dir).expect("a shim");
        let target = std::fs::read_link(dir.join(NAME)).expect("a symlink");
        assert_eq!(target, std::env::current_exe().unwrap());
    }

    #[test]
    fn a_stale_link_is_replaced() {
        // A development build and an installed one live at different paths,
        // so a link left over from the other one has to be rewritten rather
        // than trusted.
        let dir = scratch("stale");
        std::fs::create_dir_all(&dir).unwrap();
        std::os::unix::fs::symlink("/nowhere/sketchybar", dir.join(NAME)).unwrap();

        link_in(&dir).expect("a shim");
        assert_eq!(
            std::fs::read_link(dir.join(NAME)).unwrap(),
            std::env::current_exe().unwrap()
        );
    }

    #[test]
    fn asking_twice_is_not_an_error() {
        // A reload asks again, and the link is already there.
        let dir = scratch("twice");
        assert!(link_in(&dir).is_some());
        assert!(link_in(&dir).is_some());
    }

    #[test]
    fn the_shim_comes_first() {
        let path = rsbar_protocol::shimmed_path();
        let path = path.to_string_lossy();
        assert!(
            path.starts_with(&*rsbar_protocol::shim_dir().to_string_lossy()),
            "{path}"
        );
    }
}
