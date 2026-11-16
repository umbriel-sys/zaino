//! A panic in the build is caught, reported and recovered from. A test
//! storage wraps in-memory storage: its writer panics on a chosen commit of
//! a chosen index, and its open of a chosen index fails a chosen number of
//! times.

use std::path::Path;
use std::sync::{Arc, Mutex};

use zaino_indexes::IndexId;
use zaino_persistence::in_memory::{
    InMemoryBackend, InMemoryLock, InMemoryReadOnly, InMemoryReader, InMemoryStorage,
    InMemoryWriter,
};
use zaino_persistence::{
    Backend, BackendWriter, CommitError, FlushError, FootprintError, Identity, NamespaceFootprint,
    NamespaceSpec, OpenError, Storage, Verification, VerifyError, WriteOp,
};
use zaino_primitives::types::BlockRef;
use zaino_source_mock::MockChain;

use super::ingest::{every_index, oracle, stored};
use super::{chain, config, schedule, tip_of};
use crate::record::{PrimaryRecord, PrimaryState};
use crate::store::{ChainStore, Serviceability};

#[derive(Default)]
struct Faults {
    /// The index whose writer panics, and the commits it makes first.
    panic: Option<(IndexId, u32)>,
    /// The index whose opens fail, and how many more do.
    failing_opens: Option<(IndexId, u32)>,
    /// Opens per index, failed ones included.
    opens: std::collections::HashMap<IndexId, u32>,
}

#[derive(Clone, Default)]
struct Faulty {
    inner: InMemoryStorage,
    faults: Arc<Mutex<Faults>>,
}

fn index_of(dir: &Path) -> Option<IndexId> {
    let name = dir.file_name()?.to_str()?;
    IndexId::ALL
        .iter()
        .copied()
        .find(|id| id.schema().dir_name() == name)
}

impl Faulty {
    fn panic_after(&self, id: IndexId, commits: u32) {
        self.faults.lock().expect("faults").panic = Some((id, commits));
    }

    fn fail_opens(&self, id: IndexId, times: u32) {
        self.faults.lock().expect("faults").failing_opens = Some((id, times));
    }

    fn opens(&self, id: IndexId) -> u32 {
        let faults = self.faults.lock().expect("faults");
        faults.opens.get(&id).copied().unwrap_or(0)
    }
}

struct FaultyBackend {
    inner: InMemoryBackend,
    id: Option<IndexId>,
    faults: Arc<Mutex<Faults>>,
}

struct FaultyWriter {
    inner: InMemoryWriter,
    id: Option<IndexId>,
    faults: Arc<Mutex<Faults>>,
}

impl BackendWriter for FaultyWriter {
    fn commit(&mut self, tip: BlockRef, ops: Vec<WriteOp>) -> Result<(), CommitError> {
        let fire = {
            let mut faults = self.faults.lock().expect("faults");
            match &mut faults.panic {
                Some((id, 0)) if Some(*id) == self.id => {
                    faults.panic = None;
                    true
                }
                Some((id, left)) if Some(*id) == self.id => {
                    *left -= 1;
                    false
                }
                _ => false,
            }
        };
        if fire {
            panic!("injected writer panic");
        }
        self.inner.commit(tip, ops)
    }
}

impl Backend for FaultyBackend {
    type Reader = InMemoryReader;
    type Writer = FaultyWriter;

    fn reader(&self) -> Result<InMemoryReader, OpenError> {
        self.inner.reader()
    }

    fn writer(&self) -> Result<FaultyWriter, OpenError> {
        Ok(FaultyWriter {
            inner: self.inner.writer()?,
            id: self.id,
            faults: Arc::clone(&self.faults),
        })
    }

    fn flush(&self) -> Result<(), FlushError> {
        self.inner.flush()
    }

    fn verify(&self) -> Result<Verification, VerifyError> {
        self.inner.verify()
    }

    fn footprint(&self) -> Result<Vec<NamespaceFootprint>, FootprintError> {
        self.inner.footprint()
    }
}

impl Storage for Faulty {
    type Backend = FaultyBackend;
    type ReadOnly = InMemoryReadOnly;
    type Lock = InMemoryLock;

    fn open(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<FaultyBackend, OpenError> {
        let id = index_of(dir);
        {
            let mut faults = self.faults.lock().expect("faults");
            if let Some(id) = id {
                *faults.opens.entry(id).or_default() += 1;
            }
            if let Some((failing, left)) = &mut faults.failing_opens {
                if Some(*failing) == id && *left > 0 {
                    *left -= 1;
                    return Err(OpenError::Unavailable {
                        operation: "open",
                        source: "injected open failure".into(),
                    });
                }
            }
        }
        Ok(FaultyBackend {
            inner: self.inner.open(dir, identity, namespaces)?,
            id,
            faults: Arc::clone(&self.faults),
        })
    }

    fn remove(&self, dir: &Path) -> Result<(), OpenError> {
        self.inner.remove(dir)
    }

    fn children(&self, dir: &Path) -> Result<Vec<String>, OpenError> {
        self.inner.children(dir)
    }

    fn open_read_only(
        &self,
        dir: &Path,
        identity: Identity<'_>,
        namespaces: &[NamespaceSpec],
    ) -> Result<InMemoryReadOnly, OpenError> {
        self.inner.open_read_only(dir, identity, namespaces)
    }

    fn lock(&self, root: &Path) -> Result<InMemoryLock, OpenError> {
        self.inner.lock(root)
    }

    fn primary_running(&self, root: &Path) -> Result<bool, OpenError> {
        self.inner.primary_running(root)
    }

    fn write_record(&self, lock: &mut InMemoryLock, record: &[u8]) -> Result<(), OpenError> {
        self.inner.write_record(lock, record)
    }

    fn read_record(&self, root: &Path) -> Result<Option<Vec<u8>>, OpenError> {
        self.inner.read_record(root)
    }

    fn read_meta(&self, root: &Path, name: &str) -> Result<Option<Vec<u8>>, OpenError> {
        self.inner.read_meta(root, name)
    }

    fn write_meta(&self, root: &Path, name: &str, bytes: &[u8]) -> Result<(), OpenError> {
        self.inner.write_meta(root, name, bytes)
    }
}

async fn boot(storage: &Faulty, chain: &MockChain) -> ChainStore<Faulty, MockChain> {
    ChainStore::boot(
        &config(&every_index()),
        storage.clone(),
        Arc::new(chain.clone()),
        schedule(),
    )
    .await
    .expect("boots")
}

fn record(storage: &Faulty) -> PrimaryRecord {
    let bytes = storage
        .read_record(Path::new("/data/regtest"))
        .expect("read")
        .expect("written");
    PrimaryRecord::decode(&bytes).expect("decodes")
}

/// A writer that panics fails the build with the panic named, the record
/// says so, the index is reopened from disk and stays served, and the next
/// build finishes exactly as the oracle does.
#[tokio::test(flavor = "multi_thread")]
async fn a_writer_panic_is_reported_and_its_index_reopened() {
    let (storage, chain) = (Faulty::default(), chain());
    let tip = tip_of(&chain).height;
    let store = boot(&storage, &chain).await;
    storage.panic_after(IndexId::TxidLocation, 50);
    let opened = storage.opens(IndexId::TxidLocation);

    let failed = store.build_to(tip).await.expect_err("the writer panicked");
    assert_eq!(
        storage.opens(IndexId::TxidLocation),
        opened + 1,
        "reopened from disk once"
    );
    assert_eq!(
        storage.opens(IndexId::Headers),
        1,
        "the others kept their handles"
    );
    assert!(
        failed
            .to_string()
            .contains("txid_location panicked: injected writer panic"),
        "{failed}"
    );
    assert!(
        matches!(&record(&storage).state, PrimaryState::Failed(message) if message.contains("panicked")),
        "{:?}",
        record(&storage).state
    );
    assert_ne!(
        store.serviceability(IndexId::TxidLocation),
        Serviceability::Absent,
        "reopened, still enabled"
    );

    store.build_to(tip).await.expect("the next build finishes");
    assert_eq!(record(&storage).state, PrimaryState::Ready);
    let ids = every_index();
    assert_eq!(
        stored(&storage.inner, &ids),
        oracle(&chain, &ids, tip).await
    );
}

/// An index whose reopen fails stays closed (not served, never dropped)
/// and is reopened at the next build, which then finishes.
#[tokio::test(flavor = "multi_thread")]
async fn an_index_that_cannot_reopen_is_retried_at_the_next_build() {
    let (storage, chain) = (Faulty::default(), chain());
    let tip = tip_of(&chain).height;
    let store = boot(&storage, &chain).await;
    storage.panic_after(IndexId::CompactBlock, 20);
    storage.fail_opens(IndexId::CompactBlock, 1);

    let failed = store.build_to(tip).await.expect_err("the writer panicked");
    assert!(
        failed.to_string().contains("compact_block panicked"),
        "{failed}"
    );
    assert_eq!(
        store.serviceability(IndexId::CompactBlock),
        Serviceability::Building(None),
        "closed: enabled, not served"
    );
    assert_eq!(store.subscribe_watermark().borrow().tip, None);
    assert!(!record(&storage)
        .durable
        .contains_key(&IndexId::CompactBlock));

    store.build_to(tip).await.expect("reopened, then built");
    let ids = every_index();
    assert_eq!(
        stored(&storage.inner, &ids),
        oracle(&chain, &ids, tip).await
    );
}
