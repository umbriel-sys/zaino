//! In-memory backend: correct, no IO, data lost on drop.
//!
//! Useful for tests, benchmarks, and ephemeral demo runs.

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, MutexGuard};

use imbl::{OrdMap, OrdSet};
use zaino_primitives::types::BlockRef;

use crate::backend::{
    Backend, BackendReader, BackendWriter, MaintenanceReader, Namespace, NamespaceSpec, RawKey,
    RawValue, ScanRange, WriteOp,
};
use crate::chunk::next_chunk;
use crate::error::{CommitError, FlushError, FootprintError, OpenError, ReadError, VerifyError};
use crate::rules::{check_commit, check_key, check_specs, CommitState, KeyState};
use crate::verify::{NamespaceFootprint, Verification};

mod opener;
pub use opener::InMemoryOpener;

/// The current state; a commit swaps in a new one, readers keep the one they took.
type Store = Arc<Mutex<Arc<State>>>;

/// Everything committed: each namespace's entries and removed keys, and the tip.
///
/// Persistent maps: a clone is O(namespaces), sharing every entry.
#[derive(Clone)]
struct State {
    specs: Arc<[NamespaceSpec]>,
    tip: Option<BlockRef>,
    namespaces: HashMap<Namespace, Held>,
}

#[derive(Clone, Default)]
struct Held {
    entries: OrdMap<RawKey, RawValue>,
    removed: OrdSet<RawKey>,
}

/// In-memory backend. Stores key-value pairs per declared namespace.
///
/// A commit is visible and kept at once; each reader handle is a snapshot of
/// the state when it was opened.
#[derive(Clone)]
pub struct InMemoryBackend {
    data: Store,
}

impl InMemoryBackend {
    /// Create an empty backend with `namespaces` declared.
    pub fn new(namespaces: &[NamespaceSpec]) -> Result<Self, OpenError> {
        check_specs(namespaces)?;
        let held = namespaces
            .iter()
            .map(|spec| (spec.namespace, Held::default()))
            .collect();
        let state = State {
            specs: namespaces.into(),
            tip: None,
            namespaces: held,
        };
        Ok(Self {
            data: Arc::new(Mutex::new(Arc::new(state))),
        })
    }
}

/// The store, locked; a poisoned lock = a writer panicked mid-commit.
fn lock(store: &Store) -> Result<MutexGuard<'_, Arc<State>>, ReadError> {
    store.lock().map_err(|_| ReadError::ReadFailed {
        operation: "lock",
        source: "in-memory store lock poisoned".into(),
    })
}

/// The current state, shared.
fn current(store: &Store) -> Result<Arc<State>, ReadError> {
    Ok(Arc::clone(&*lock(store)?))
}

/// The held data of `namespace`, or `NamespaceNotFound` if it was not declared.
fn declared(state: &State, namespace: Namespace) -> Result<&Held, ReadError> {
    state
        .namespaces
        .get(&namespace)
        .ok_or_else(|| ReadError::NamespaceNotFound(namespace.to_string()))
}

impl CommitState for State {
    fn tip(&self) -> Option<BlockRef> {
        self.tip
    }

    fn sequence_len(&self, namespace: Namespace) -> Result<u64, ReadError> {
        Ok(declared(self, namespace)?.entries.len() as u64)
    }

    fn key_state(&self, namespace: Namespace, key: &[u8]) -> Result<KeyState, ReadError> {
        let held = declared(self, namespace)?;
        Ok(if held.entries.contains_key(key) {
            KeyState::Present
        } else if held.removed.contains(key) {
            KeyState::Removed
        } else {
            KeyState::Absent
        })
    }
}

impl Backend for InMemoryBackend {
    type Reader = InMemoryReader;
    type Writer = InMemoryWriter;

    fn reader(&self) -> Result<Self::Reader, OpenError> {
        let state = current(&self.data).map_err(|source| OpenError::Unavailable {
            operation: "snapshot",
            source: source.into(),
        })?;
        Ok(InMemoryReader { state })
    }

    fn writer(&self) -> Result<Self::Writer, OpenError> {
        Ok(InMemoryWriter {
            data: Arc::clone(&self.data),
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        Ok(())
    }

    /// Nothing is stored, so there is nothing to check: the tip and the bytes held, no unit.
    fn verify(&self) -> Result<Verification, VerifyError> {
        let state = current(&self.data).map_err(|source| VerifyError::Failed {
            operation: "snapshot",
            source: source.into(),
        })?;
        Ok(Verification {
            tip: state.tip,
            committed_bytes: state.namespaces.values().map(held_bytes).sum(),
            units: Vec::new(),
        })
    }

    fn footprint(&self) -> Result<Vec<NamespaceFootprint>, FootprintError> {
        let state = current(&self.data).map_err(|source| FootprintError::Failed {
            operation: "snapshot",
            source: source.into(),
        })?;
        Ok(state
            .specs
            .iter()
            .map(|spec| NamespaceFootprint {
                namespace: spec.namespace,
                bytes: state.namespaces.get(&spec.namespace).map_or(0, held_bytes),
                cached: None,
            })
            .collect())
    }
}

/// Key and value bytes `held` stores.
fn held_bytes(held: &Held) -> u64 {
    held.entries
        .iter()
        .map(|(k, v)| (k.len() + v.len()) as u64)
        .sum()
}

/// Read handle for the in-memory backend: the state when it was opened.
pub struct InMemoryReader {
    state: Arc<State>,
}

impl BackendReader for InMemoryReader {
    fn tip(&self) -> Result<Option<BlockRef>, ReadError> {
        Ok(self.state.tip)
    }

    /// Every commit is kept at once: the tip.
    fn durable_tip(&self) -> Result<Option<BlockRef>, ReadError> {
        Ok(self.state.tip)
    }

    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        check_key(&self.state.specs, namespace, key)?;
        Ok(declared(&self.state, namespace)?.entries.get(key).cloned())
    }

    fn with_value<T>(
        &self,
        namespace: Namespace,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, ReadError> {
        check_key(&self.state.specs, namespace, key)?;
        Ok(declared(&self.state, namespace)?
            .entries
            .get(key)
            .map(|value| read(value)))
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
        InMemoryChunks {
            state: Arc::clone(&self.state),
            namespace,
            range,
            filter,
            done: false,
            _item: PhantomData,
        }
    }
}

impl MaintenanceReader for InMemoryReader {
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        Ok(declared(&self.state, namespace)?
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect())
    }
}

/// Lazy chunk iterator for [`InMemoryReader::scan_range`].
///
/// Walks the reader's snapshot, resuming past the last visited key.
struct InMemoryChunks<T, F> {
    state: Arc<State>,
    namespace: Namespace,
    range: ScanRange,
    filter: F,
    done: bool,
    _item: PhantomData<fn() -> T>,
}

impl<T, F> Iterator for InMemoryChunks<T, F>
where
    F: FnMut(&[u8], &[u8]) -> Option<T>,
{
    type Item = Result<Vec<T>, ReadError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let map = match declared(&self.state, self.namespace) {
            Ok(held) => &held.entries,
            Err(err) => {
                self.done = true;
                return Some(Err(err));
            }
        };
        next_chunk(
            &mut self.range,
            &mut self.done,
            &mut self.filter,
            |lower, upper| {
                map.range::<_, [u8]>((lower, upper))
                    .map(|(k, v)| (k.as_slice(), v.as_slice()))
            },
        )
        .map(Ok)
    }
}

/// Write handle for the in-memory backend.
pub struct InMemoryWriter {
    data: Store,
}

impl BackendWriter for InMemoryWriter {
    fn commit(&mut self, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        // poisoned = a writer panicked mid-commit
        let mut guard = self.data.lock().map_err(|_| CommitError::Poisoned)?;
        // Check the whole batch first so a rejected batch applies nothing.
        check_commit(&guard.specs, &**guard, tip, &ops)?;
        // Built aside and swapped in whole: readers keep theirs, a failure applies nothing.
        let mut state = State::clone(&guard);
        for op in ops {
            let (WriteOp::Put { namespace, .. } | WriteOp::Delete { namespace, .. }) = &op;
            let held = state
                .namespaces
                .get_mut(namespace)
                .ok_or_else(|| CommitError::NamespaceNotFound(namespace.to_string()))?;
            match op {
                WriteOp::Put { key, value, .. } => {
                    held.entries.insert(key, value);
                }
                WriteOp::Delete { key, .. } => {
                    held.entries.remove(&key);
                    held.removed.insert(key);
                }
            }
        }
        state.tip = Some(tip);
        *guard = Arc::new(state);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
