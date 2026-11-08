//! [`BackendOpener`] over LSM-tree directories (D-75).

use std::{num::NonZeroUsize, path::Path, sync::Arc};

use zaino_persistence::{BackendOpener, Identity, NamespaceSpec, OpenError};

use crate::{fs::Fs, remove::remove_dir_all, LsmConfig, LsmTreeBackend, LsmTreeConfig};

/// Opens every index directory with one engine configuration.
#[derive(Debug, Clone)]
pub struct LsmTreeOpener {
    /// The filesystem the directories live on.
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

impl BackendOpener for LsmTreeOpener {
    type Backend = LsmTreeBackend;

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
        remove_dir_all(self.fs.as_ref(), dir).map_err(|source| OpenError::Unavailable {
            operation: "remove directory",
            source: source.into(),
        })
    }

    fn children(&self, dir: &Path) -> Result<Vec<String>, OpenError> {
        let unavailable = |source: std::io::Error| OpenError::Unavailable {
            operation: "list directories",
            source: source.into(),
        };
        if !self.fs.is_dir(dir).map_err(unavailable)? {
            return Ok(Vec::new());
        }
        let mut dirs = Vec::new();
        for name in self.fs.list(dir).map_err(unavailable)? {
            if self.fs.is_dir(&dir.join(&name)).map_err(unavailable)? {
                dirs.push(name);
            }
        }
        Ok(dirs)
    }
}

#[cfg(test)]
mod tests {
    use std::{num::NonZeroUsize, path::Path, sync::Arc};

    use zaino_persistence::{
        conformance::{Model, SPECS},
        Backend as _, BackendOpener as _, BackendReader as _, Identity, OpenError,
    };

    use super::LsmTreeOpener;
    use crate::{fs::SimFs, testing::commit, LsmConfig};

    fn opener(fs: &Arc<SimFs>) -> LsmTreeOpener {
        LsmTreeOpener {
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
        let opener = opener(&fs);
        let root = Path::new("/store/indexes");
        let dir = root.join("conformance_1");
        let backend = opener.open(&dir, identity(1), &SPECS).expect("created");
        commit(&backend, Model::default().advance(1, &[1], 1, &[])).expect("commit");
        backend.flush().expect("flush");
        let tip = backend.reader().expect("reader").tip().expect("tip");
        assert!(opener.remove(&dir).is_err(), "refused while open");
        drop(backend);

        let reopened = opener.open(&dir, identity(1), &SPECS).expect("reopened");
        assert_eq!(reopened.reader().expect("reader").tip().expect("tip"), tip);
        drop(reopened);
        assert!(matches!(
            opener.open(&dir, identity(2), &SPECS),
            Err(OpenError::Mismatch(_))
        ));

        assert_eq!(opener.children(root).expect("listed"), ["conformance_1"]);
        opener.remove(&dir).expect("removed");
        assert!(opener.children(root).expect("listed").is_empty());
        assert!(opener
            .children(Path::new("/nowhere"))
            .expect("listed")
            .is_empty());
    }
}
