//! [`BackendOpener`] over in-memory backends: directories are map keys.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use super::InMemoryBackend;
use crate::backend::{NamespaceSpec, SchemaDigest};
use crate::error::OpenError;
use crate::opener::{BackendOpener, Identity};

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

type Dirs = HashMap<PathBuf, (Stamp, InMemoryBackend)>;

/// In-memory "directories": a reopen returns the same data; a removal
/// cannot tell whether a handle is still held.
#[derive(Clone, Default)]
pub struct InMemoryOpener {
    dirs: Arc<Mutex<Dirs>>,
}

impl InMemoryOpener {
    /// No directories yet.
    pub fn new() -> Self {
        Self::default()
    }

    fn dirs(&self, operation: &'static str) -> Result<MutexGuard<'_, Dirs>, OpenError> {
        self.dirs.lock().map_err(|_| OpenError::Unavailable {
            operation,
            source: "in-memory opener lock poisoned".into(),
        })
    }
}

impl BackendOpener for InMemoryOpener {
    type Backend = InMemoryBackend;

    fn open(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<InMemoryBackend, OpenError> {
        let stamp = Stamp::new(identity, namespaces);
        let mut dirs = self.dirs("open")?;
        match dirs.get(dir) {
            Some((held, backend)) if *held == stamp => Ok(backend.clone()),
            Some((held, _)) => Err(OpenError::Mismatch(
                format!("{} holds {held:?}, opened as {stamp:?}", dir.display()).into(),
            )),
            None => {
                let backend = InMemoryBackend::new(namespaces)?;
                dirs.insert(dir.to_path_buf(), (stamp, backend.clone()));
                Ok(backend)
            }
        }
    }

    fn remove(&self, dir: &Path) -> Result<(), OpenError> {
        self.dirs("remove")?.remove(dir);
        Ok(())
    }

    fn children(&self, dir: &Path) -> Result<Vec<String>, OpenError> {
        let mut names: Vec<_> = self
            .dirs("children")?
            .keys()
            .filter(|path| path.parent() == Some(dir))
            .filter_map(|path| path.file_name()?.to_str().map(str::to_owned))
            .collect();
        names.sort_unstable();
        Ok(names)
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::InMemoryOpener;
    use crate::conformance::SPECS;
    use crate::{BackendOpener as _, Identity, OpenError, SchemaDigest};

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
        let opener = InMemoryOpener::new();
        let dir = Path::new("/store/indexes/index_1");
        opener.open(dir, identity(1), &SPECS).expect("created");
        opener.open(dir, identity(1), &SPECS).expect("reopened");
        assert!(matches!(
            opener.open(dir, identity(2), &SPECS),
            Err(OpenError::Mismatch(_))
        ));
        assert!(matches!(
            opener.open(dir, identity(1), &SPECS[..1]),
            Err(OpenError::Mismatch(_))
        ));
    }

    #[test]
    fn children_list_and_removal_forgets() {
        let opener = InMemoryOpener::new();
        let root = Path::new("/store/indexes");
        for name in ["b_1", "a_1"] {
            opener
                .open(&root.join(name), identity(1), &SPECS)
                .expect("created");
        }
        assert_eq!(opener.children(root).expect("listed"), ["a_1", "b_1"]);
        opener.remove(&root.join("a_1")).expect("removed");
        opener.remove(&root.join("absent")).expect("absent is fine");
        assert_eq!(opener.children(root).expect("listed"), ["b_1"]);
        assert!(opener
            .children(Path::new("/nowhere"))
            .expect("listed")
            .is_empty());
    }
}
