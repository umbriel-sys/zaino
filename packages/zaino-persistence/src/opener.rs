//! Opening a backend per directory (D-75): what a multi-index store asks of
//! its storage.

use std::path::Path;

use crate::backend::{Backend, NamespaceSpec, SchemaDigest};
use crate::error::OpenError;

/// What a directory is stamped with when created and checked against at
/// every open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
    /// The index's name.
    pub name: &'a str,
    /// The chain its data belongs to (network, genesis hash, regtest
    /// activations; opaque here).
    pub chain: &'a str,
    /// The layout version.
    pub format: u16,
    /// The digest of every namespace's layout.
    pub schema: SchemaDigest,
}

/// Opens, lists and removes backend directories.
pub trait BackendOpener: Send + Sync + 'static {
    /// The backend it opens.
    type Backend: Backend;

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
}
