//! The temp directory zebra's read-only state leaves behind.
//!
//! Opening zebra's state read-only opens a RocksDB secondary instance in a
//! fresh `zebra-secondary_state-…` directory under the system temp
//! directory, which zebra never removes (zebra-state `disk_db.rs`). An
//! [`Opening`] brackets the open and hands back the directories it created;
//! they are removed when the [`SecondaryStateDirs`] drops.

use std::collections::HashSet;
use std::fs::File;
use std::path::{Path, PathBuf};

/// The name zebra gives the directory, less its random suffix.
const PREFIX: &str = "zebra-secondary_state-";

/// Held across every process's open, so a directory is attributed to the
/// open that made it.
const LOCK_FILE: &str = "zaino-zebra-secondary.lock";

/// One open of zebra's read-only state, between [`begin`](Self::begin) and
/// [`finish`](Self::finish).
pub struct Opening {
    root: PathBuf,
    before: HashSet<PathBuf>,
    /// Released on drop.
    _lock: File,
}

impl Opening {
    /// Starts an open. Blocks while another open is in progress, in this or
    /// any other process.
    pub fn begin() -> std::io::Result<Self> {
        Self::begin_in(&std::env::temp_dir())
    }

    fn begin_in(root: &Path) -> std::io::Result<Self> {
        let lock = File::create(root.join(LOCK_FILE))?;
        lock.lock()?;
        Ok(Self {
            root: root.to_path_buf(),
            before: secondary_dirs(root)?,
            _lock: lock,
        })
    }

    /// Ends the open: the secondary directories it created.
    pub fn finish(self) -> std::io::Result<SecondaryStateDirs> {
        let created = secondary_dirs(&self.root)?
            .into_iter()
            .filter(|dir| !self.before.contains(dir))
            .collect();
        Ok(SecondaryStateDirs(created))
    }
}

/// Every secondary state directory under `root`.
fn secondary_dirs(root: &Path) -> std::io::Result<HashSet<PathBuf>> {
    let mut dirs = HashSet::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        if entry.file_name().to_string_lossy().starts_with(PREFIX) {
            dirs.insert(entry.path());
        }
    }
    Ok(dirs)
}

/// The secondary directories one open created, removed on drop.
#[derive(Debug)]
pub struct SecondaryStateDirs(Vec<PathBuf>);

impl Drop for SecondaryStateDirs {
    fn drop(&mut self) {
        for dir in &self.0 {
            if let Err(_error) = std::fs::remove_dir_all(dir) {
                #[cfg(feature = "tracing")]
                tracing::warn!(dir = %dir.display(), error = %_error, "secondary state directory not removed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only the directories created during the open are taken, and they are
    /// removed on drop; one that was already there is left alone.
    #[test]
    fn an_open_owns_only_the_directories_it_created() {
        let root = tempfile::tempdir().expect("temp dir");
        let earlier = root.path().join(format!("{PREFIX}v27-regtest-earlier"));
        let unrelated = root.path().join("zebra-state-v27-regtest-other");
        std::fs::create_dir(&earlier).expect("created");

        let opening = Opening::begin_in(root.path()).expect("begins");
        let created = root.path().join(format!("{PREFIX}v27-regtest-created"));
        std::fs::create_dir(&created).expect("created");
        std::fs::write(created.join("LOG"), b"x").expect("written");
        std::fs::create_dir(&unrelated).expect("created");
        let dirs = opening.finish().expect("finishes");

        assert!(created.exists());
        drop(dirs);
        assert!(!created.exists(), "removed with its contents");
        assert!(earlier.exists(), "another open's directory");
        assert!(unrelated.exists(), "not a secondary directory");
    }

    /// A second open waits for the first to finish.
    #[test]
    fn opens_are_serialised() {
        let root = tempfile::tempdir().expect("temp dir");
        let first = Opening::begin_in(root.path()).expect("begins");
        let contender = File::create(root.path().join(LOCK_FILE)).expect("opens");
        assert!(contender.try_lock().is_err(), "held by the first open");
        drop(first);
        assert!(contender.try_lock().is_ok(), "released");
    }
}
