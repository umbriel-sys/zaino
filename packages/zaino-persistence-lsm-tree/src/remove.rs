//! Removing an index directory (D-84): crash-safe, refused while open.
//!
//! The commit point is `<dir>/REMOVING`, created and made durable first.
//! Before it the directory is intact; after it, whatever a crash leaves is
//! finished by the next [`remove_dir_all`] or open. Every other entry's
//! removal is synced before the marker goes, so no crash keeps data without
//! the marker.

use std::{io, path::Path};

use crate::{dir::LOCK, fs::Fs};

/// The marker naming a directory as being removed (a reserved name).
pub(crate) const REMOVING: &str = "REMOVING";

/// `dir` and everything under it removed; `Ok` when absent. Refused
/// (`WouldBlock`) while a backend holds the directory open.
pub fn remove_dir_all(fs: &dyn Fs, dir: &Path) -> io::Result<()> {
    if !fs.is_dir(dir)? {
        return Ok(());
    }
    let lock_path = dir.join(LOCK);
    let lock = fs.lock(&lock_path)?;
    let marker = dir.join(REMOVING);
    if fs.open_existing(&marker)?.is_none() {
        fs.open(&marker)?;
        fs.sync_dir(dir)?;
    }
    remove_entries(fs, dir, &[REMOVING, LOCK])?;
    fs.remove(&marker)?;
    fs.remove(&lock_path)?;
    drop(lock);
    fs.remove_dir(dir)?;
    fs.sync_dir(dir.parent().unwrap_or(Path::new(".")))
}

/// Whether `dir` carries the marker: a removal a crash interrupted.
pub(crate) fn interrupted(fs: &dyn Fs, dir: &Path) -> io::Result<bool> {
    Ok(fs.is_dir(dir)? && fs.open_read_only(&dir.join(REMOVING))?.is_some())
}

/// Every entry of `dir` but `keep` removed, subdirectories whole, the
/// removals durable on return.
fn remove_entries(fs: &dyn Fs, dir: &Path, keep: &[&str]) -> io::Result<()> {
    for name in fs.list(dir)? {
        if keep.contains(&name.as_str()) {
            continue;
        }
        let path = dir.join(&name);
        match fs.is_dir(&path)? {
            true => {
                remove_entries(fs, &path, &[])?;
                fs.remove_dir(&path)?;
            }
            false => fs.remove(&path)?,
        }
    }
    fs.sync_dir(dir)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use zaino_persistence::{conformance::Model, Backend as _, BackendReader as _};

    use super::{remove_dir_all, REMOVING};
    use crate::{
        fs::{Fs as _, SimFs},
        testing::{commit, open, ROOT},
        LsmTreeBackend,
    };

    /// A flushed directory with merged map segments (subdirectories) in it.
    fn built(fs: &std::sync::Arc<SimFs>) -> Option<zaino_primitives::types::BlockRef> {
        let backend = open(fs, 2);
        let mut model = Model::default();
        for n in 0..6u8 {
            commit(&backend, model.advance(1, &[n % 3], 1, &[])).expect("commit");
            backend.flush().expect("flush");
        }
        let tip = backend.reader().expect("reader").tip().expect("tip");
        drop(backend);
        tip
    }

    #[test]
    fn a_directory_is_removed_whole() {
        let fs = SimFs::new();
        built(&fs);
        assert!(fs.list(Path::new(ROOT)).expect("listed").len() > 3);
        remove_dir_all(fs.as_ref(), Path::new(ROOT)).expect("removed");
        assert!(!fs.is_dir(Path::new(ROOT)).expect("checked"));
        remove_dir_all(fs.as_ref(), Path::new(ROOT)).expect("absent is fine");
    }

    #[test]
    fn an_open_directory_is_refused() {
        let fs = SimFs::new();
        let backend = open(&fs, 2);
        let refused = remove_dir_all(fs.as_ref(), Path::new(ROOT)).expect_err("held");
        assert_eq!(refused.kind(), std::io::ErrorKind::WouldBlock);
        drop(backend);
    }

    /// At every crash point of a removal, reopening finds the directory
    /// either whole (its tip intact) or gone (empty): never part of it.
    #[test]
    fn a_crash_leaves_the_directory_whole_or_removed() {
        let fs = SimFs::recording();
        let tip = built(&fs);
        assert!(tip.is_some());
        fs.set_tag(1);
        remove_dir_all(fs.as_ref(), Path::new(ROOT)).expect("removed");
        let mut seen = (0, 0);
        for state in fs.crash_states().into_iter().filter(|state| state.tag == 1) {
            let reopened = LsmTreeBackend::open(
                state.fs.clone(),
                Path::new(ROOT),
                &crate::testing::config(2, std::num::NonZeroUsize::MAX),
            )
            .unwrap_or_else(|error| panic!("{}: {error}", state.label));
            let now = reopened.reader().expect("reader").tip().expect("tip");
            match now {
                None => seen.1 += 1,
                now if now == tip => seen.0 += 1,
                now => panic!("{}: tip {now:?}, neither {tip:?} nor empty", state.label),
            }
            assert!(
                state
                    .fs
                    .open_read_only(&Path::new(ROOT).join(REMOVING))
                    .expect("read")
                    .is_none(),
                "{}: open left the marker",
                state.label
            );
        }
        assert!(
            seen.0 > 0 && seen.1 > 0,
            "whole {}, removed {}",
            seen.0,
            seen.1
        );
    }
}
