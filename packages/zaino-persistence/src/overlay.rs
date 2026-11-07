//! An in-memory layer of commits above a durable root, read through the
//! port (D-53).
//!
//! An [`Overlay`] holds what the blocks above its root wrote, in persistent
//! `imbl` maps of shared bytes, so deriving the next block's overlay from
//! its parent's costs a node copy per write and each version stays readable.
//! It answers only for writes above the root: a key below it reads as
//! absent, and serving composes the two tiers. With a genesis root (`None`)
//! it is a complete store and enforces every commit rule; above a block it
//! cannot see the keys below, so a sequence starts at its first append and a
//! delete of an unseen key is a key below the root. [`rebase`](Overlay::rebase)
//! drops the blocks a new durable root covers by replaying the rest.

use std::collections::HashMap;
use std::sync::Arc;

use imbl::{OrdMap, OrdSet, Vector};
use zaino_primitives::types::BlockRef;

use crate::backend::{
    sequence_position, BackendReader, MaintenanceReader, Namespace, NamespaceKind, NamespaceSpec,
    RawKey, RawValue, ScanRange, WriteOp,
};
use crate::chunk::next_chunk;
use crate::error::{CommitError, OpenError, ReadError};
use crate::rules::{
    check_key, check_keys, check_shape, check_specs, ranged, CommitState, KeyState,
};

/// Shared bytes: cloning a map node clones pointers, not data.
type Shared = Arc<[u8]>;

/// One namespace's writes above the root.
#[derive(Debug, Clone, Default)]
struct Held {
    entries: OrdMap<Shared, Shared>,
    removed: OrdSet<Shared>,
    /// A sequence's first position above the root.
    base: Option<u64>,
}

/// One block's writes, kept to replay on a rebase.
#[derive(Debug)]
struct Delta {
    block: BlockRef,
    ops: Vec<WriteOp>,
}

/// Commits above a durable root; cloning is O(namespaces).
#[derive(Debug, Clone)]
pub struct Overlay {
    specs: Arc<[NamespaceSpec]>,
    root: Option<BlockRef>,
    tip: Option<BlockRef>,
    namespaces: HashMap<Namespace, Held>,
    deltas: Vector<Arc<Delta>>,
}

/// Why an overlay cannot be rebased.
#[derive(Debug, thiserror::Error)]
pub enum RebaseError {
    /// The new root is not one of the overlay's blocks nor its root.
    #[error("rebase onto a block this overlay does not hold")]
    NotHeld,
    /// Replaying the blocks above the new root was refused.
    #[error("replay")]
    Replay(#[from] CommitError),
}

impl Overlay {
    /// An empty overlay over `root` (`None` = genesis) declaring `specs`.
    pub fn new(specs: &[NamespaceSpec], root: Option<BlockRef>) -> Result<Self, OpenError> {
        check_specs(specs)?;
        Ok(Self {
            specs: specs.into(),
            root,
            tip: None,
            namespaces: specs
                .iter()
                .map(|spec| (spec.namespace, Held::default()))
                .collect(),
            deltas: Vector::new(),
        })
    }

    /// The durable root it sits on.
    pub fn root(&self) -> Option<BlockRef> {
        self.root
    }

    /// This overlay plus `block`'s `ops`, checked as a commit; `self` is
    /// unchanged and shares its data with the result.
    pub fn with(&self, block: BlockRef, ops: Vec<WriteOp>) -> Result<Self, CommitError> {
        let mut next = self.clone();
        next.learn_bases(&ops);
        check_shape(&self.specs, &next, block, &ops)?;
        match self.root {
            None => check_keys(&self.specs, &next, &ops)?,
            Some(_) => next.check_keys_above_root(&ops)?,
        }
        for op in &ops {
            next.apply(op)?;
        }
        next.tip = Some(block);
        next.deltas.push_back(Arc::new(Delta { block, ops }));
        Ok(next)
    }

    /// This overlay over `root` instead: the blocks at or below it dropped,
    /// the rest replayed. `root` must be the current root or one of its
    /// blocks. Costs the replayed blocks' writes, so a caller rebases in
    /// batches.
    pub fn rebase(&self, root: BlockRef) -> Result<Self, RebaseError> {
        if self.root == Some(root) {
            return Ok(self.clone());
        }
        if !self.deltas.iter().any(|delta| delta.block == root) {
            return Err(RebaseError::NotHeld);
        }
        let mut next = Self {
            specs: Arc::clone(&self.specs),
            root: Some(root),
            tip: None,
            namespaces: self
                .specs
                .iter()
                .map(|spec| (spec.namespace, Held::default()))
                .collect(),
            deltas: Vector::new(),
        };
        for delta in self
            .deltas
            .iter()
            .filter(|delta| delta.block.height > root.height)
        {
            next = next.with(delta.block, delta.ops.clone())?;
        }
        Ok(next)
    }

    /// Sets each sequence's base on its first append: 0 over genesis, else
    /// the append's position.
    fn learn_bases(&mut self, ops: &[WriteOp]) {
        let over_genesis = self.root.is_none();
        for op in ops {
            let WriteOp::Put { namespace, key, .. } = op else {
                continue;
            };
            let is_sequence = self.specs.iter().any(|spec| {
                spec.namespace == *namespace && matches!(spec.kind, NamespaceKind::Sequence { .. })
            });
            if let (true, Some(held)) = (is_sequence, self.namespaces.get_mut(namespace)) {
                if held.base.is_none() {
                    held.base = match over_genesis {
                        true => Some(0),
                        false => sequence_position(key),
                    };
                }
            }
        }
    }

    /// Above a block: a put needs a key neither held nor removed here; a
    /// delete of a key never seen here is a key below the root.
    fn check_keys_above_root(&self, ops: &[WriteOp]) -> Result<(), CommitError> {
        for op in ops {
            let (namespace, key, is_put) = match op {
                WriteOp::Put { namespace, key, .. } => (namespace, key, true),
                WriteOp::Delete { namespace, key } => (namespace, key, false),
            };
            let is_map = self.specs.iter().any(|spec| {
                spec.namespace == *namespace && matches!(spec.kind, NamespaceKind::Map { .. })
            });
            if !is_map {
                continue;
            }
            let name = || namespace.to_string();
            match (
                is_put,
                self.key_state(*namespace, key)
                    .map_err(CommitError::from_check)?,
            ) {
                (_, KeyState::Removed) => {
                    return Err(CommitError::KeyRemoved { namespace: name() })
                }
                (true, KeyState::Present) => {
                    return Err(CommitError::KeyExists { namespace: name() })
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn apply(&mut self, op: &WriteOp) -> Result<(), CommitError> {
        let (namespace, key) = match op {
            WriteOp::Put { namespace, key, .. } | WriteOp::Delete { namespace, key } => {
                (namespace, key)
            }
        };
        let held = self
            .namespaces
            .get_mut(namespace)
            .ok_or_else(|| CommitError::NamespaceNotFound(namespace.to_string()))?;
        let key: Shared = key.as_slice().into();
        match op {
            WriteOp::Put { value, .. } => {
                held.entries.insert(key, value.as_slice().into());
            }
            WriteOp::Delete { .. } => {
                held.entries.remove(&key);
                held.removed.insert(key);
            }
        }
        Ok(())
    }

    fn held(&self, namespace: Namespace) -> Result<&Held, ReadError> {
        self.namespaces
            .get(&namespace)
            .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
    }
}

impl CommitState for Overlay {
    fn tip(&self) -> Option<BlockRef> {
        self.tip.or(self.root)
    }

    fn sequence_len(&self, namespace: Namespace) -> Result<u64, ReadError> {
        let held = self.held(namespace)?;
        let count = u64::try_from(held.entries.len()).unwrap_or(u64::MAX);
        Ok(held.base.unwrap_or(0).saturating_add(count))
    }

    fn key_state(&self, namespace: Namespace, key: &[u8]) -> Result<KeyState, ReadError> {
        let held = self.held(namespace)?;
        Ok(if held.entries.contains_key(key) {
            KeyState::Present
        } else if held.removed.contains(key) {
            KeyState::Removed
        } else {
            KeyState::Absent
        })
    }
}

impl BackendReader for Overlay {
    /// The last block applied, else the root.
    fn tip(&self) -> Result<Option<BlockRef>, ReadError> {
        Ok(CommitState::tip(self))
    }

    /// Nothing here survives a crash: the root.
    fn durable_tip(&self) -> Result<Option<BlockRef>, ReadError> {
        Ok(self.root)
    }

    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        self.with_value(namespace, key, <[u8]>::to_vec)
    }

    fn with_value<T>(
        &self,
        namespace: Namespace,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, ReadError> {
        check_key(&self.specs, namespace, key)?;
        Ok(self
            .held(namespace)?
            .entries
            .get(key)
            .map(|value| read(value)))
    }

    fn get_bytes(
        &self,
        namespace: Namespace,
        key: &[u8],
        range: core::ops::Range<usize>,
    ) -> Result<Option<RawValue>, ReadError> {
        self.with_value(namespace, key, |value| {
            ranged(namespace, value, &range).map(<[u8]>::to_vec)
        })?
        .transpose()
    }

    fn scan_range<T, F>(
        &self,
        namespace: Namespace,
        range: ScanRange,
        filter: F,
    ) -> impl Iterator<Item = Result<Vec<T>, ReadError>> + Send + 'static + use<T, F>
    where
        T: Send + 'static,
        F: FnMut(&[u8], &[u8]) -> Option<T> + Send + 'static,
    {
        let (entries, error) = match self.held(namespace) {
            Ok(held) => (held.entries.clone(), None),
            Err(error) => (OrdMap::new(), Some(error)),
        };
        OverlayChunks {
            entries,
            error,
            range,
            filter,
            done: false,
        }
    }
}

impl MaintenanceReader for Overlay {
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        Ok(self
            .held(namespace)?
            .entries
            .iter()
            .map(|(key, value)| (key.to_vec(), value.to_vec()))
            .collect())
    }
}

#[cfg(test)]
mod tests;

/// Lazy chunks of one namespace's entries, over a snapshot of them.
struct OverlayChunks<F> {
    entries: OrdMap<Shared, Shared>,
    /// An undeclared namespace, yielded once.
    error: Option<ReadError>,
    range: ScanRange,
    filter: F,
    done: bool,
}

impl<T, F> Iterator for OverlayChunks<F>
where
    F: FnMut(&[u8], &[u8]) -> Option<T>,
{
    type Item = Result<Vec<T>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        if let Some(error) = self.error.take() {
            self.done = true;
            return Some(Err(error));
        }
        let entries = &self.entries;
        next_chunk(
            &mut self.range,
            &mut self.done,
            &mut self.filter,
            |lower, upper| {
                entries
                    .range::<_, [u8]>((lower, upper))
                    .map(|(key, value)| (&**key, &**value))
            },
        )
        .map(Ok)
    }
}
