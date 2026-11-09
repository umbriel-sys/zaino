//! What a multi-index store asks of its storage (D-75): one backend
//! directory per index, plus the store's lock and metadata files.

use std::path::Path;

use crate::backend::{Backend, NamespaceSpec, SchemaDigest};
use crate::error::OpenError;

/// What a directory is stamped with when created and checked against at
/// every open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
    /// The index's name.
    pub name: &'a str,
    /// The chain its data belongs to (opaque here).
    pub chain: &'a str,
    /// The layout version.
    pub format: u16,
    /// The digest of every namespace's layout.
    pub schema: SchemaDigest,
}

/// Index directories and store files, all a store touches on disk.
pub trait Storage: Send + Sync + 'static {
    /// The backend it opens.
    type Backend: Backend;
    /// Held while the store runs; dropping it releases the store.
    type Lock: Send + Sync + 'static;

    /// `dir` (created if absent) as `identity` holding `namespaces`; a
    /// directory stamped otherwise is [`OpenError::Mismatch`].
    fn open(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<Self::Backend, OpenError>;

    /// `dir` and everything in it removed (`Ok` when absent); refused while
    /// a backend holds it open, where the storage can tell.
    fn remove(&self, dir: &Path) -> Result<(), OpenError>;

    /// The names of the directories directly under `dir` (none when absent).
    fn children(&self, dir: &Path) -> Result<Vec<String>, OpenError>;

    /// The store at `root` (created if absent) held exclusively; another
    /// holder is refused.
    fn lock(&self, root: &Path) -> Result<Self::Lock, OpenError>;

    /// File `name` under `root`; `None` when absent.
    fn read_meta(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, OpenError>;

    /// File `name` under `root` replaced by `bytes` atomically and durably:
    /// a crash leaves the old bytes or the new, whole.
    fn write_meta(&self, root: &Path, name: &str, bytes: &[u8]) -> Result<(), OpenError>;
}
