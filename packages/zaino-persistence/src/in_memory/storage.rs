//! [`Storage`] in memory: directories, locks and files are map entries.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::InMemoryBackend;
use crate::backend::{NamespaceSpec, SchemaDigest};
use crate::error::OpenError;
use crate::storage::{Identity, Storage};

/// What a directory was created as.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    name: String,
    chain: String,
    format: u16,
    schema: SchemaDigest,
    namespaces: Vec<NamespaceSpec>,
}

impl Stamp {
    fn new(identity: Identity<'_>, namespaces: &[NamespaceSpec]) -> Self {
        Self {
            name: identity.name.to_owned(),
            chain: identity.chain.to_owned(),
            format: identity.format,
            schema: identity.schema,
            namespaces: namespaces.to_vec(),
        }
    }
}

#[derive(Default)]
struct State {
    dirs: HashMap<PathBuf, (Stamp, InMemoryBackend)>,
    locks: HashSet<PathBuf>,
    files: HashMap<PathBuf, Vec<u8>>,
}

type Shared = Arc<Mutex<State>>;

/// In-memory storage: a reopen returns the same data; a removal cannot tell
/// whether a handle is still held.
#[derive(Clone, Default)]
pub struct InMemoryStorage {
    state: Shared,
}

/// A held store; dropping it releases the root.
pub struct InMemoryLock {
    state: Shared,
    root: PathBuf,
}

impl Drop for InMemoryLock {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            state.locks.remove(&self.root);
        }
    }
}

fn locked<'a>(
    state: &'a Shared,
    operation: &'static str,
) -> Result<MutexGuard<'a, State>, OpenError> {
    state.lock().map_err(|_| OpenError::Unavailable {
        operation,
        source: "in-memory storage lock poisoned".into(),
    })
}

impl InMemoryStorage {
    /// Nothing stored yet.
    pub fn new() -> Self {
        Self::default()
    }
}

impl Storage for InMemoryStorage {
    type Backend = InMemoryBackend;
    type Lock = InMemoryLock;

    fn open(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<InMemoryBackend, OpenError> {
        let stamp = Stamp::new(identity, namespaces);
        let mut state = locked(&self.state, "open")?;
        match state.dirs.get(dir) {
            Some((held, backend)) if *held == stamp => Ok(backend.clone()),
            Some((held, _)) => Err(OpenError::Mismatch(
                format!("{} holds {held:?}, opened as {stamp:?}", dir.display()).into(),
            )),
            None => {
                let backend = InMemoryBackend::new(namespaces)?;
                state
                    .dirs
                    .insert(dir.to_path_buf(), (stamp, backend.clone()));
                Ok(backend)
            }
        }
    }

    fn remove(&self, dir: &Path) -> Result<(), OpenError> {
        locked(&self.state, "remove")?.dirs.remove(dir);
        Ok(())
    }

    fn children(&self, dir: &Path) -> Result<Vec<String>, OpenError> {
        let mut names: Vec<_> = locked(&self.state, "children")?
            .dirs
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
            .collect();
        names.sort_unstable();
        Ok(names)
    }

    fn lock(&self, root: &Path) -> Result<InMemoryLock, OpenError> {
        match locked(&self.state, "lock")?
            .locks
            .insert(root.to_path_buf())
        {
            true => Ok(InMemoryLock {
                state: Arc::clone(&self.state),
                root: root.to_path_buf(),
            }),
            false => Err(OpenError::Unavailable {
                operation: "lock",
                source: format!("{} is held", root.display()).into(),
            }),
        }
    }

    fn read_meta(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, OpenError> {
        Ok(locked(&self.state, "read meta")?
            .files
            .get(&root.join(name))
            .cloned())
    }

    fn write_meta(&self, root: &Path, name: &str, bytes: &[u8]) -> Result<(), OpenError> {
        locked(&self.state, "write meta")?
            .files
            .insert(root.join(name), bytes.to_vec());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::InMemoryStorage;
    use crate::conformance::SPECS;
    use crate::{Identity, OpenError, SchemaDigest, Storage as _};

    fn identity(format: u16) -> Identity<'static> {
        Identity {
            name: "index",
            chain: "regtest",
            format,
            schema: SchemaDigest::new([1; 32]),
        }
    }

    #[test]
    fn a_directory_reopens_as_itself_and_refuses_another_identity() {
        let storage = InMemoryStorage::new();
        let dir = Path::new("/store/indexes/index_1");
        storage.open(dir, identity(1), &SPECS).expect("created");
        storage.open(dir, identity(1), &SPECS).expect("reopened");
        assert!(matches!(
            storage.open(dir, identity(2), &SPECS),
            Err(OpenError::Mismatch(_))
        ));
        assert!(matches!(
            storage.open(dir, identity(1), &SPECS[..1]),
            Err(OpenError::Mismatch(_))
        ));
    }

    #[test]
    fn children_list_and_removal_forgets() {
        let storage = InMemoryStorage::new();
        let root = Path::new("/store/indexes");
        for name in ["b_1", "a_1"] {
            storage
                .open(&root.join(name), identity(1), &SPECS)
                .expect("created");
        }
        assert_eq!(storage.children(root).expect("listed"), ["a_1", "b_1"]);
        storage.remove(&root.join("a_1")).expect("removed");
        storage
            .remove(&root.join("absent"))
            .expect("absent is fine");
        assert_eq!(storage.children(root).expect("listed"), ["b_1"]);
        assert!(storage
            .children(Path::new("/nowhere"))
            .expect("listed")
            .is_empty());
    }

    #[test]
    fn the_lock_is_exclusive_until_dropped_and_meta_round_trips() {
        let storage = InMemoryStorage::new();
        let root = Path::new("/store");
        let held = storage.lock(root).expect("locked");
        assert!(storage.lock(root).is_err(), "held");
        drop(held);
        drop(storage.lock(root).expect("released"));

        assert_eq!(storage.read_meta(root, "METADATA").expect("read"), None);
        storage
            .write_meta(root, "METADATA", b"one")
            .expect("written");
        storage
            .write_meta(root, "METADATA", b"two")
            .expect("replaced");
        assert_eq!(
            storage.read_meta(root, "METADATA").expect("read"),
            Some(b"two".to_vec())
        );
    }
}
