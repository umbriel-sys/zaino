//! [`Storage`] over LSM-tree directories on an [`Fs`] (D-75).

use std::{io, num::NonZeroUsize, path::Path, sync::Arc};

use zaino_persistence::{Identity, NamespaceSpec, OpenError, Storage};

use crate::{
    dir::{create_dir_durable, LOCK},
    fs::{Fs, LockGuard},
    remove::remove_dir_all,
    LsmConfig, LsmTreeBackend, LsmTreeConfig,
};

/// Index directories and store files on one filesystem, every directory
/// opened with one engine configuration (default tuning).
#[derive(Debug, Clone)]
pub struct LsmTreeStorage {
    /// The filesystem everything lives on.
    pub fs: Arc<dyn Fs>,
    /// The maps' merge policy.
    pub lsm: LsmConfig,
    /// Buffered heap at which a commit writes the buffer.
    pub write_buffer: NonZeroUsize,
    /// Each backend's pool threads (`None` = available parallelism).
    pub threads: Option<NonZeroUsize>,
    /// Refuse a manifest readable only through the page cache.
    pub strict_manifest_read: bool,
}

fn unavailable(operation: &'static str) -> impl FnOnce(io::Error) -> OpenError {
    move |source| OpenError::Unavailable {
        operation,
        source: source.into(),
    }
}

impl Storage for LsmTreeStorage {
    type Backend = LsmTreeBackend;
    type Lock = LockGuard;

    fn open(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<LsmTreeBackend, OpenError> {
        let config = LsmTreeConfig {
            name: identity.name,
            chain: identity.chain,
            format: identity.format,
            schema: identity.schema,
            namespaces,
            tuning: &[],
            lsm: self.lsm,
            write_buffer: self.write_buffer,
            threads: self.threads,
            strict_manifest_read: self.strict_manifest_read,
        };
        LsmTreeBackend::open(Arc::clone(&self.fs), dir, &config)
    }

    fn remove(&self, dir: &Path) -> Result<(), OpenError> {
        remove_dir_all(self.fs.as_ref(), dir).map_err(unavailable("remove directory"))
    }

    fn children(&self, dir: &Path) -> Result<Vec<String>, OpenError> {
        if !self.fs.is_dir(dir).map_err(unavailable("list"))? {
            return Ok(Vec::new());
        }
        let mut dirs = Vec::new();
        for name in self.fs.list(dir).map_err(unavailable("list"))? {
            if self
                .fs
                .is_dir(&dir.join(&name))
                .map_err(unavailable("list"))?
            {
                dirs.push(name);
            }
        }
        Ok(dirs)
    }

    fn lock(&self, root: &Path) -> Result<LockGuard, OpenError> {
        create_dir_durable(self.fs.as_ref(), root).map_err(unavailable("create store"))?;
        self.fs
            .lock(&root.join(LOCK))
            .map_err(unavailable("lock store"))
    }

    fn read_meta(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, OpenError> {
        let read = || -> io::Result<Option<Vec<u8>>> {
            let Some(file) = self.fs.open_read_only(&root.join(name))? else {
                return Ok(None);
            };
            let len = usize::try_from(file.len()?).map_err(io::Error::other)?;
            let mut bytes = vec![0; len];
            file.read_exact_at(&mut bytes, 0)?;
            Ok(Some(bytes))
        };
        read().map_err(unavailable("read meta"))
    }

    /// Written whole to `<name>.tmp`, synced, renamed over `name`, the
    /// rename synced.
    fn write_meta(&self, root: &Path, name: &str, bytes: &[u8]) -> Result<(), OpenError> {
        let write = || -> io::Result<()> {
            create_dir_durable(self.fs.as_ref(), root)?;
            let temp = root.join(format!("{name}.tmp"));
            let file = self.fs.open(&temp)?;
            file.set_len(0)?;
            file.write_all_at(bytes, 0)?;
            file.sync_data()?;
            self.fs.rename(&temp, &root.join(name))?;
            self.fs.sync_dir(root)
        };
        write().map_err(unavailable("write meta"))
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path, sync::Arc};

    use zaino_persistence::{
        conformance::{Model, SPECS},
        Backend as _, BackendReader as _, Identity, OpenError, Storage as _,
    };

    use super::LsmTreeStorage;
    use crate::{fs::SimFs, testing::commit, LsmConfig};

    fn storage(fs: &Arc<SimFs>) -> LsmTreeStorage {
        LsmTreeStorage {
            fs: fs.clone(),
            lsm: LsmConfig::default(),
            write_buffer: NonZeroUsize::MAX,
            threads: NonZeroUsize::new(2),
            strict_manifest_read: false,
        }
    }

    fn identity(format: u16) -> Identity<'static> {
        Identity {
            name: "conformance",
            chain: "regtest",
            format,
            schema: crate::testing::SCHEMA,
        }
    }

    #[test]
    fn open_reopen_list_and_remove() {
        let fs = SimFs::new();
        let storage = storage(&fs);
        let root = Path::new("/store/indexes");
        let dir = root.join("conformance_1");
        let backend = storage.open(&dir, identity(1), &SPECS).expect("created");
        commit(&backend, Model::default().advance(1, &[1], 1, &[])).expect("commit");
        backend.flush().expect("flush");
        let tip = backend.reader().expect("reader").tip().expect("tip");
        assert!(storage.remove(&dir).is_err(), "refused while open");
        drop(backend);

        let reopened = storage.open(&dir, identity(1), &SPECS).expect("reopened");
        assert_eq!(reopened.reader().expect("reader").tip().expect("tip"), tip);
        drop(reopened);
        assert!(matches!(
            storage.open(&dir, identity(2), &SPECS),
            Err(OpenError::Mismatch(_))
        ));

        assert_eq!(storage.children(root).expect("listed"), ["conformance_1"]);
        storage.remove(&dir).expect("removed");
        assert!(storage.children(root).expect("listed").is_empty());
        assert!(storage
            .children(Path::new("/nowhere"))
            .expect("listed")
            .is_empty());
    }

    #[test]
    fn the_store_lock_is_exclusive_until_dropped() {
        let fs = SimFs::new();
        let storage = storage(&fs);
        let root = Path::new("/store");
        let held = storage.lock(root).expect("locked");
        assert!(storage.lock(root).is_err(), "held");
        drop(held);
        drop(storage.lock(root).expect("released"));
    }

    /// A shorter rewrite never keeps the old tail, and at every crash point
    /// of a rewrite the file holds the old bytes or the new, whole.
    #[test]
    fn meta_is_replaced_whole_through_any_crash() {
        let fs = SimFs::recording();
        let storage = storage(&fs);
        let root = Path::new("/store");
        let (old, new) = (b"the old metadata".to_vec(), b"new".to_vec());
        assert_eq!(storage.read_meta(root, "META").expect("read"), None);
        storage.write_meta(root, "META", &old).expect("written");
        fs.set_tag(1);
        storage.write_meta(root, "META", &new).expect("replaced");
        assert_eq!(
            storage.read_meta(root, "META").expect("read"),
            Some(new.clone())
        );
        let mut seen = (false, false);
        for state in fs.crash_states().into_iter().filter(|state| state.tag == 1) {
            let read = super::LsmTreeStorage {
                fs: state.fs.clone(),
                ..storage.clone()
            }
            .read_meta(root, "META")
            .expect("read");
            match read {
                Some(bytes) if bytes == old => seen.0 = true,
                Some(bytes) if bytes == new => seen.1 = true,
                other => panic!("{}: {other:?}", state.label),
            }
        }
        assert!(seen.0 && seen.1, "old {}, new {}", seen.0, seen.1);
    }
}
