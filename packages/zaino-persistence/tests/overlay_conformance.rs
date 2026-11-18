//! The port's conformance suite against an [`Overlay`] over genesis: a
//! complete store, every commit kept in memory.

use std::ops::Range;
use std::sync::{Arc, Mutex};

use proptest::prelude::*;
use zaino_persistence::{
    conformance::{contract, history, steps, Subject, SPECS},
    Backend, BackendReader, BackendWriter, CommitError, FlushError, FootprintError,
    MaintenanceReader, Namespace, NamespaceFootprint, OpenError, Overlay, RawKey, RawValue,
    ReadError, ScanRange, Verification, VerifyError, WriteOp,
};
use zaino_primitives::types::BlockRef;

/// The current overlay; a commit swaps in its successor.
#[derive(Clone)]
struct Shared(Arc<Mutex<Overlay>>);

impl Shared {
    fn new() -> Self {
        let overlay = Overlay::new(&SPECS, None).expect("conformance specs are distinct");
        Self(Arc::new(Mutex::new(overlay)))
    }

    fn current(&self) -> Overlay {
        self.0
            .lock()
            .expect("no test thread panics holding it")
            .clone()
    }

    fn bytes(&self, namespace: Namespace) -> u64 {
        let entries = self.current().scan(namespace).expect("declared");
        entries
            .iter()
            .map(|(k, v)| (k.len() + v.len()) as u64)
            .sum()
    }
}

/// A snapshot; the suite's store keeps everything, so its tip is durable.
struct Snapshot(Overlay);

impl BackendReader for Snapshot {
    fn tip(&self) -> Result<Option<BlockRef>, ReadError> {
        self.0.tip()
    }

    fn durable_tip(&self) -> Result<Option<BlockRef>, ReadError> {
        self.0.tip()
    }

    fn get(&self, namespace: Namespace, key: &[u8]) -> Result<Option<RawValue>, ReadError> {
        self.0.get(namespace, key)
    }

    fn with_value<T>(
        &self,
        namespace: Namespace,
        key: &[u8],
        read: impl FnOnce(&[u8]) -> T,
    ) -> Result<Option<T>, ReadError> {
        self.0.with_value(namespace, key, read)
    }

    fn get_bytes(
        &self,
        namespace: Namespace,
        key: &[u8],
        range: Range<usize>,
    ) -> Result<Option<RawValue>, ReadError> {
        self.0.get_bytes(namespace, key, range)
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
        self.0.scan_range(namespace, range, filter)
    }
}

impl MaintenanceReader for Snapshot {
    fn scan(&self, namespace: Namespace) -> Result<Vec<(RawKey, RawValue)>, ReadError> {
        self.0.scan(namespace)
    }
}

impl BackendWriter for Shared {
    fn commit(&mut self, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        let mut current = self.0.lock().map_err(|_| CommitError::Poisoned)?;
        *current = current.with(tip, ops)?;
        Ok(())
    }
}

impl Backend for Shared {
    type Reader = Snapshot;
    type Writer = Shared;

    fn reader(&self) -> Result<Snapshot, OpenError> {
        Ok(Snapshot(self.current()))
    }

    fn writer(&self) -> Result<Shared, OpenError> {
        Ok(self.clone())
    }

    fn flush(&self) -> Result<(), FlushError> {
        Ok(())
    }

    fn verify(&self) -> Result<Verification, VerifyError> {
        Ok(Verification {
            tip: self.current().tip().expect("an overlay's tip never fails"),
            committed_bytes: SPECS.iter().map(|spec| self.bytes(spec.namespace)).sum(),
            units: Vec::new(),
        })
    }

    fn footprint(&self) -> Result<Vec<NamespaceFootprint>, FootprintError> {
        Ok(SPECS
            .iter()
            .map(|spec| NamespaceFootprint {
                namespace: spec.namespace,
                bytes: self.bytes(spec.namespace),
                cached: None,
            })
            .collect())
    }
}

/// One shared overlay: a reopen is a fresh handle onto it.
struct OverOverlay(Shared);

impl Subject for OverOverlay {
    type Backend = Shared;

    fn open(&self) -> Shared {
        self.0.clone()
    }

    fn keeps_unflushed(&self) -> bool {
        true
    }
}

#[test]
fn overlay_contract() {
    contract(OverOverlay(Shared::new()));
}

proptest! {
    #[test]
    fn overlay_history(steps in steps()) {
        history(OverOverlay(Shared::new()), &steps);
    }
}
