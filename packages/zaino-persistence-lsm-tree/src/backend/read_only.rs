//! [`LsmTreeReadOnly`]: an index directory another process writes, read-only (§5.2a)
//!
//! - no lock, nothing created, truncated or removed: the writer owns the directory
//! - each refresh maps the manifest slot at the writer's announced durable tip, so nothing the
//!   writer has not made durable is ever served

use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use arc_swap::ArcSwapOption;
use zaino_persistence::{
    check_specs, NamespaceSpec, OpenError, ReadOnlyBackend, RefreshError, SchemaDigest,
};
use zaino_primitives::types::BlockRef;

use super::{LsmTreeReader, ReadState, Versions};
use crate::{
    buffer::Buffer,
    emit,
    engine::{self, check_names, Linked},
    error::{guarded, StoreError},
    fs::Fs,
    manifest::Identity,
};

/// A directory another process writes, mapped read-only at its writer's announced durable tip
pub struct LsmTreeReadOnly {
    fs: Arc<dyn Fs>,
    path: PathBuf,
    identity: Identity,
    specs: Arc<[NamespaceSpec]>,
    versions: Arc<Versions>,
    published: ArcSwapOption<ReadState>,
    /// One refresh at a time (each maps from the last)
    refreshing: Mutex<()>,
}

impl LsmTreeReadOnly {
    /// `path` linked as index `name` of `chain` at `format` / `schema`, holding `namespaces`;
    /// nothing read until the first refresh
    pub fn open(
        fs: Arc<dyn Fs>,
        path: &Path,
        name: &str,
        chain: &str,
        format: u16,
        schema: SchemaDigest,
        namespaces: &[NamespaceSpec],
    ) -> Result<Self, OpenError> {
        check_specs(namespaces)?;
        let checked = || -> Result<Identity, StoreError> {
            check_names(namespaces)?;
            Ok(Identity::new(name, chain, format, schema)?)
        };
        let identity = checked().map_err(|error| error.into_open("open read-only"))?;
        let (live, heap) = emit::versions(name);
        Ok(Self {
            fs,
            path: path.to_path_buf(),
            identity,
            specs: namespaces.into(),
            versions: Arc::new(Versions { live, heap }),
            published: ArcSwapOption::empty(),
            refreshing: Mutex::new(()),
        })
    }

    fn refreshed(&self, durable: Option<BlockRef>) -> Result<Result<(), RefreshError>, StoreError> {
        let _one = self
            .refreshing
            .lock()
            .map_err(|_| StoreError::Internal("refresh lock poisoned"))?;
        let previous = self.published.load_full();
        // the same tip is the same data, on whatever files
        if previous
            .as_ref()
            .is_some_and(|state| state.view.tip() == durable)
        {
            return Ok(Ok(()));
        }
        let previous_view = previous.as_ref().map(|state| &state.view);
        let linked = engine::linked(
            self.fs.as_ref(),
            &self.path,
            &self.identity,
            &self.specs,
            durable,
            previous_view,
        )?;
        let view = match linked {
            Linked::View(view) => view,
            Linked::Superseded => return Ok(Err(RefreshError::Superseded)),
            Linked::Absent => return Ok(Err(RefreshError::Absent)),
        };
        self.published.store(Some(Arc::new(ReadState::new(
            Arc::clone(&self.specs),
            view,
            Buffer::empty(&self.specs),
            &self.versions,
        ))));
        Ok(Ok(()))
    }
}

impl ReadOnlyBackend for LsmTreeReadOnly {
    type Reader = LsmTreeReader;

    fn reader(&self) -> Result<LsmTreeReader, OpenError> {
        let state = self.published.load_full().ok_or(OpenError::Unavailable {
            operation: "read-only reader",
            source: "not yet refreshed".into(),
        })?;
        Ok(LsmTreeReader { state })
    }

    fn refresh(&self, durable: Option<BlockRef>) -> Result<(), RefreshError> {
        guarded(|| self.refreshed(durable))
            .and_then(|refreshed| refreshed)
            .map_err(|error| RefreshError::Open(error.into_open("refresh")))?
    }
}
