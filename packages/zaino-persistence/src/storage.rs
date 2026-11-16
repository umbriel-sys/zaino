//! What a multi-index store asks of its storage (D-75): one backend
//! directory per index, plus the store's lock and metadata files.

use std::path::Path;

use crate::backend::{Backend, NamespaceSpec, ReadOnlyBackend, SchemaDigest};
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
///
/// One process, the primary, holds the store and writes it; others link to
/// it read-only (§5.2a): they read the primary's record and open its index
/// directories with [`open_read_only`](Self::open_read_only).
pub trait Storage: Send + Sync + 'static {
    /// The backend it opens.
    type Backend: Backend;
    /// The read-only backend it opens.
    type ReadOnly: ReadOnlyBackend;
    /// Held while the primary runs; dropping it releases the store.
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

    /// `dir` linked read-only as `identity` holding `namespaces`: nothing
    /// created, locked or removed; the directory is read at each refresh.
    fn open_read_only(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<Self::ReadOnly, OpenError>;

    /// The store at `root` (created if absent) held exclusively, as its
    /// primary; another primary is refused.
    fn lock(&self, root: &Path) -> Result<Self::Lock, OpenError>;

    /// Whether a primary holds the store at `root` now (it releases the store
    /// however it ends, crash included).
    fn primary_running(&self, root: &Path) -> Result<bool, OpenError>;

    /// The primary's record replaced in place: a reader sees the old record
    /// or the new, whole. Not synced: a crash may keep an older record.
    fn write_record(&self, lock: &mut Self::Lock, record: &[u8]) -> Result<(), OpenError>;

    /// The last record a primary of `root` wrote; `None` when none has.
    fn read_record(&self, root: &Path) -> Result<Option<Vec<u8>>, OpenError>;

    /// File `name` under `root`; `None` when absent.
    fn read_meta(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, OpenError>;

    /// File `name` under `root` replaced by `bytes` atomically and durably:
    /// a crash leaves the old bytes or the new, whole.
    fn write_meta(&self, root: &Path, name: &str, bytes: &[u8]) -> Result<(), OpenError>;
}
