//! The store runtime (§5.2, D-72, D-78): boot over a [`Storage`], then
//! [`ChainStore::build_to`], the only write path.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::{watch, Mutex};
use zaino_chain_store::{
    ChainStoreError, ChainStoreIngest, ChainStoreService, ChainStoreSource, ChainStoreSourceError,
    Provenance, StoreCapabilities, StoreCapability, StoreWatermark,
};
use zaino_component::{ComponentStatus, StatusSource};
use zaino_indexes::{FoldError, IndexId, IndexState};
use zaino_persistence::{Backend, BackendReader, Identity, OpenError, ReadError, Storage};
use zaino_primitives::types::{BlockHash, BlockRef, Height, NetworkUpgradeInfo};

use crate::config::{network_name, ChainStoreServiceConfig, IngestConfig, OnCorruption};
use crate::ingest::{self, Ingest};
use crate::metadata::{self, Metadata};
use crate::reader::StoreReader;
use crate::record::{status_of, PrimaryRecord, PrimaryState};
use crate::set::{IndexSet, SetError};

/// How far one index can answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serviceability {
    /// Not enabled.
    Absent,
    /// Enabled, built to the height given (`None` = nothing yet), still
    /// building toward the last target.
    Building(Option<Height>),
    /// Built to the last target, at the height given.
    ToHeight(Height),
}

/// Why the store did not start.
#[derive(Debug, thiserror::Error)]
pub enum BootError {
    /// The index set is malformed.
    #[error("index set")]
    Set(#[from] SetError),
    /// Storage refused (the store held elsewhere, a directory unopenable).
    #[error("{index}: storage")]
    Storage {
        /// The index or `"store"`.
        index: &'static str,
        /// Why.
        #[source]
        source: OpenError,
    },
    /// An index's data is corrupt and `on_corruption = "halt"`.
    #[error("{index}: corrupted, halted (on_corruption = halt)")]
    Halted {
        /// The index.
        index: &'static str,
        /// What was found.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    /// The validator could not answer the open-time tip check (D-32).
    #[error("tip check")]
    Source(#[from] ChainStoreSourceError),
}

/// One open index.
pub(crate) struct Open<B: Backend> {
    pub(crate) id: IndexId,
    dir: PathBuf,
    pub(crate) backend: Arc<B>,
    pub(crate) writer: B::Writer,
    pub(crate) state: Option<IndexState>,
    pub(crate) tip: Option<BlockRef>,
    /// Its tip is above the validator's, so not yet checked (D-32): not
    /// served or built until it is.
    unverified: bool,
}

/// Everything `build_to` changes, behind one lock (single-flight).
struct Built<St: Storage> {
    indexes: Vec<Open<St::Backend>>,
    /// Indexes whose writer panicked and whose reopen has not yet succeeded.
    closed: Vec<Closed>,
    target: Option<Height>,
    recorder: Recorder<St>,
    /// Shut down: builds are refused.
    shut: bool,
}

/// An index to reopen from disk: not served, retried at every build.
struct Closed {
    id: IndexId,
    dir: PathBuf,
}

/// The held store and the record linked readers follow (D-95); the state is
/// also published in-process, for the store's status.
struct Recorder<St: Storage> {
    lock: St::Lock,
    record: PrimaryRecord,
    state: watch::Sender<PrimaryState>,
}

impl<St: Storage> Recorder<St> {
    fn new(lock: St::Lock, state: watch::Sender<PrimaryState>) -> Self {
        Self {
            lock,
            record: PrimaryRecord {
                state: PrimaryState::Booting,
                target: None,
                durable: BTreeMap::new(),
            },
            state,
        }
    }

    fn set(&mut self, storage: &St, state: PrimaryState) {
        self.state.send_replace(state.clone());
        self.record.state = state;
        self.write(storage);
    }

    /// Each verified index's durable tip (an unverified one is not served,
    /// so not announced), rewritten when one moved.
    fn durable<'a>(
        &mut self,
        storage: &St,
        indexes: impl Iterator<Item = (IndexId, &'a St::Backend)>,
    ) where
        St::Backend: 'a,
    {
        let mut durable = BTreeMap::new();
        for (id, backend) in indexes {
            let tip = match backend.reader().map(|reader| reader.durable_tip()) {
                Ok(Ok(tip)) => tip,
                // unreadable now: the last announcement stands
                _ => self.record.durable.get(&id).copied().flatten(),
            };
            durable.insert(id, tip);
        }
        if durable != self.record.durable {
            self.record.durable = durable;
            self.write(storage);
        }
    }

    /// Advisory: a failed write leaves linked readers on the last record.
    fn write(&mut self, storage: &St) {
        if let Err(error) = storage.write_record(&mut self.lock, &self.record.encode()) {
            tracing::warn!(%error, "primary record not written: linked readers keep the last one");
        }
    }
}

/// The verified indexes and their backends: what the record announces.
fn verified<B: Backend>(indexes: &[Open<B>]) -> impl Iterator<Item = (IndexId, &B)> {
    indexes
        .iter()
        .filter(|index| !index.unverified)
        .map(|index| (index.id, &*index.backend))
}

/// `error` and each cause under it, on one line.
fn causes(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        line.push_str(": ");
        line.push_str(&cause.to_string());
        source = cause.source();
    }
    line
}

/// What every handle onto one running store shares.
pub(crate) struct Inner<St: Storage, Src> {
    storage: St,
    network: &'static str,
    source: Arc<Src>,
    schedule: Arc<[NetworkUpgradeInfo]>,
    set: IndexSet,
    trees: bool,
    ingest: IngestConfig,
    on_corruption: OnCorruption,
    built: Mutex<Built<St>>,
    watermark: watch::Sender<StoreWatermark>,
    serviceability: watch::Sender<BTreeMap<IndexId, Serviceability>>,
    /// Each enabled index's backend, for readers (which never take `built`).
    backends: std::sync::RwLock<BTreeMap<IndexId, Arc<St::Backend>>>,
    state: watch::Receiver<PrimaryState>,
}

impl<St: Storage, Src: ChainStoreSource> Inner<St, Src> {
    /// Boots the store at `<root>/<network>/`: locks it, resolves the set,
    /// deletes stale directories (`delete_stale`), opens each index (a schema
    /// mismatch is rebuilt, corruption per `on_corruption`), checks each tip
    /// against the validator (D-32: a different block is rebuilt), records
    /// `METADATA`.
    ///
    /// The record says `Booting` while it runs and `Halted` if it fails.
    pub async fn boot(
        config: &ChainStoreServiceConfig,
        storage: St,
        source: Arc<Src>,
        schedule: Arc<[NetworkUpgradeInfo]>,
    ) -> Result<Self, BootError> {
        let network = network_name(config.network);
        let root = config.root.join(network);
        let lock = storage.lock(&root).map_err(|source| BootError::Storage {
            index: "store",
            source,
        })?;
        let (state_tx, state) = watch::channel(PrimaryState::Booting);
        let mut recorder = Recorder::new(lock, state_tx);
        recorder.set(&storage, PrimaryState::Booting);
        match Self::open_set(config, &storage, &source, &root, &mut recorder).await {
            Ok((set, indexes)) => {
                recorder.durable(&storage, verified(&indexes));
                recorder.set(&storage, PrimaryState::Ready);
                let tips = || indexes.iter().map(|index| (index.id, served_tip(index)));
                let (watermark, _) = watch::channel(watermark_of(tips()));
                let (serviceability, _) = watch::channel(serviceability_of(tips(), None));
                let backends = std::sync::RwLock::new(backends_of(&indexes));
                let built = Built {
                    indexes,
                    closed: Vec::new(),
                    shut: false,
                    target: None,
                    recorder,
                };
                Ok(Self {
                    storage,
                    network,
                    source,
                    schedule,
                    trees: set.contains(IndexId::TreeState),
                    ingest: config.ingest.clone(),
                    on_corruption: config.on_corruption,
                    set,
                    built: Mutex::new(built),
                    watermark,
                    serviceability,
                    backends,
                    state,
                })
            }
            Err(error) => {
                recorder.set(&storage, PrimaryState::Halted(causes(&error)));
                Err(error)
            }
        }
    }

    /// The set resolved, its directories swept and opened, tips checked,
    /// `METADATA` recorded.
    async fn open_set(
        config: &ChainStoreServiceConfig,
        storage: &St,
        source: &Arc<Src>,
        root: &Path,
        recorder: &mut Recorder<St>,
    ) -> Result<(IndexSet, Vec<Open<St::Backend>>), BootError> {
        let network = network_name(config.network);
        let set = IndexSet::resolve(
            config.indexes.set,
            &config.indexes.enable,
            &config.indexes.disable,
        )?;
        let parents = parents(config, root, &set);
        sweep(storage, &parents, &set, config.indexes.delete_stale)?;
        report_change(storage, root, network, &set);

        let mut indexes = Vec::new();
        for id in set.build_order() {
            let dir = parent_of(&parents, id).join(id.schema().dir_name());
            indexes.push(open(
                storage,
                &dir,
                id,
                network,
                config.on_corruption,
                recorder,
            )?);
        }
        if indexes.iter().any(|index| index.tip.is_some()) {
            for index in &mut indexes {
                index.unverified = index.tip.is_some();
            }
            check_tips(storage, source.as_ref(), network, &mut indexes, recorder)
                .await
                .map_err(|error| match error {
                    TipCheck::Source(error) => BootError::Source(error),
                    TipCheck::Rebuild(error) => error,
                })?;
        }
        let record = Metadata::of(network, set.iter())
            .encode()
            .map_err(|error| BootError::Storage {
                index: "store",
                source: OpenError::Unavailable {
                    operation: "encode metadata",
                    source: error.into(),
                },
            })?;
        storage
            .write_meta(root, metadata::NAME, &record)
            .map_err(|source| BootError::Storage {
                index: "store",
                source,
            })?;
        Ok((set, indexes))
    }

    /// Builds every enabled index through `target`, one block at a time
    /// (single-flight: a concurrent call waits, then finds the work done).
    /// Each block is fetched once and folded into every index still below
    /// it; the indexes are flushed at the end.
    ///
    /// The record says `Building`, then `Ready`; a validator that does not
    /// answer is `Stalled`, a failed fold or write `Failed`. Each index's
    /// durable tip is announced as it moves.
    pub async fn build_to(&self, target: Height) -> Result<(), ChainStoreSourceError>
    where
        St::Backend: 'static,
        <St::Backend as Backend>::Writer: 'static,
    {
        let mut built = self.built.lock().await;
        let built = &mut *built;
        if built.shut {
            return Err(ChainStoreSourceError::Commit {
                message: "the store is shut down".to_owned(),
                cause: None,
            });
        }
        built.target = Some(built.target.map_or(target, |old| old.max(target)));
        built.recorder.record.target = built.target;
        built.recorder.set(&self.storage, PrimaryState::Building);
        let result = self.build(built, target).await;
        let state = match &result {
            Ok(()) => PrimaryState::Ready,
            Err(
                error @ (ChainStoreSourceError::Unavailable { .. }
                | ChainStoreSourceError::NotReady { .. }),
            ) => PrimaryState::Stalled(causes(error)),
            Err(error) => PrimaryState::Failed(causes(error)),
        };
        built
            .recorder
            .durable(&self.storage, verified(&built.indexes));
        built.recorder.set(&self.storage, state);
        result
    }

    /// The pipeline over every verified index; one building from prevouts
    /// waits while I-03 or I-04 is held back.
    async fn build(
        &self,
        built: &mut Built<St>,
        target: Height,
    ) -> Result<(), ChainStoreSourceError>
    where
        St::Backend: 'static,
        <St::Backend as Backend>::Writer: 'static,
    {
        // a closed index still failing to reopen fails this build, after it runs
        let before = self.reopen(built);
        if built.indexes.iter().any(|index| index.unverified) {
            check_tips(
                &self.storage,
                self.source.as_ref(),
                self.network,
                &mut built.indexes,
                &mut built.recorder,
            )
            .await
            .map_err(|error| match error {
                TipCheck::Source(error) => error,
                TipCheck::Rebuild(error) => ChainStoreSourceError::Commit {
                    message: "rebuilding an index on another chain".to_owned(),
                    cause: Some(Box::new(error)),
                },
            })?;
            self.publish(built);
        }
        let Built {
            indexes,
            closed,
            target: wanted,
            recorder,
            ..
        } = built;
        let prevouts_held = indexes.iter().any(|index| {
            index.unverified && matches!(index.id, IndexId::TxidLocation | IndexId::Transparent)
        });
        let (building, waiting): (Vec<_>, Vec<_>) =
            std::mem::take(indexes).into_iter().partition(|index| {
                let waits = prevouts_held && index.id.builds_from().next().is_some();
                !(index.unverified || waits)
            });
        let ingest = Ingest {
            source: &self.source,
            schedule: &self.schedule,
            trees: self.trees,
            config: &self.ingest,
        };
        // what progress needs of the waiting and closed indexes, not the
        // indexes themselves (their writers stay off the pipeline's futures)
        let still: Vec<(IndexId, Option<BlockRef>)> = waiting
            .iter()
            .map(|index| (index.id, served_tip(index)))
            .chain(closed.iter().map(|index| (index.id, None)))
            .collect();
        let still_verified: Vec<(IndexId, Arc<St::Backend>)> = waiting
            .iter()
            .filter(|index| !index.unverified)
            .map(|index| (index.id, Arc::clone(&index.backend)))
            .collect();
        let (returned, result) = ingest::build(building, target, ingest, |running| {
            let applied = running
                .iter()
                .map(|index| (index.id, *index.applied.borrow()));
            self.publish_tips(applied.chain(still.iter().copied()), *wanted);
            let backends = running
                .iter()
                .map(|index| (index.id, &*index.backend))
                .chain(still_verified.iter().map(|(id, backend)| (*id, &**backend)));
            // a backend writes its buffer on its own once full
            recorder.durable(&self.storage, backends);
        })
        .await;
        let mut kept = waiting;
        for back in returned {
            match back.panicked {
                // its in-memory state is unknown: dropped, reopened from disk
                true => closed.push(Closed {
                    id: back.open.id,
                    dir: back.open.dir.clone(),
                }),
                false => kept.push(back.open),
            }
        }
        *indexes = kept;
        let reopened = self.reopen(built);
        self.publish(built);
        result.and(reopened).and(before)
    }

    /// Every closed index reopened from disk as boot opens it (a mismatch
    /// or corruption rebuilt per `on_corruption`), its tip checked against
    /// the validator at the next build; one that fails stays closed, its
    /// failure returned.
    fn reopen(&self, built: &mut Built<St>) -> Result<(), ChainStoreSourceError> {
        let mut failure = None;
        for closed in std::mem::take(&mut built.closed) {
            match open(
                &self.storage,
                &closed.dir,
                closed.id,
                self.network,
                self.on_corruption,
                &mut built.recorder,
            ) {
                Ok(mut reopened) => {
                    tracing::info!(
                        index = closed.id.name(),
                        "reopened after its writer panicked"
                    );
                    reopened.unverified = reopened.tip.is_some();
                    built.indexes.push(reopened);
                }
                Err(error) => {
                    tracing::error!(index = closed.id.name(), error = %causes(&error), "reopen failed: retried at the next build");
                    failure.get_or_insert(ChainStoreSourceError::Commit {
                        message: format!("{} reopen", closed.id.name()),
                        cause: Some(Box::new(error)),
                    });
                    built.closed.push(closed);
                }
            }
        }
        let order = self.set.build_order();
        built
            .indexes
            .sort_by_key(|index| order.iter().position(|id| *id == index.id));
        failure.map_or(Ok(()), Err)
    }

    /// The enabled set.
    pub fn set(&self) -> &IndexSet {
        &self.set
    }

    /// Watches the store watermark: the lowest tip among the enabled
    /// indexes (empty when any is).
    pub fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.watermark.subscribe()
    }

    /// Watches each index's serviceability.
    pub fn subscribe_serviceability(&self) -> watch::Receiver<BTreeMap<IndexId, Serviceability>> {
        self.serviceability.subscribe()
    }

    /// `id`'s serviceability now.
    pub fn serviceability(&self, id: IndexId) -> Serviceability {
        self.serviceability
            .borrow()
            .get(&id)
            .copied()
            .unwrap_or(Serviceability::Absent)
    }

    /// What the store answers now (D-81): a capability once each of its
    /// indexes holds a block.
    pub fn capabilities(&self) -> StoreCapabilities {
        capabilities_of(&self.serviceability.borrow())
    }

    fn publish(&self, built: &Built<St>) {
        let tips = built
            .indexes
            .iter()
            .map(|index| (index.id, served_tip(index)))
            .chain(built.closed.iter().map(|index| (index.id, None)));
        self.publish_tips(tips, built.target);
        *self
            .backends
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = backends_of(&built.indexes);
    }

    /// A reader over every enabled index, answering up to the watermark
    /// taken first (so each index holds at least that much).
    fn reader(&self) -> StoreReader<<St::Backend as Backend>::Reader> {
        let watermark = *self.watermark.borrow();
        let capabilities = self.capabilities();
        let backends = self
            .backends
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let readers = backends
            .into_iter()
            .filter_map(|(id, backend)| match backend.reader() {
                Ok(reader) => Some((id, reader)),
                Err(error) => {
                    tracing::warn!(index = id.name(), %error, "no reader: its reads are unavailable");
                    None
                }
            })
            .collect();
        StoreReader::new(readers, watermark, capabilities, self.status())
    }

    /// The store's status, from its state and how far it has built.
    fn status(&self) -> ComponentStatus {
        let tip = self.watermark.borrow().tip.map(|tip| tip.height);
        let target = self
            .serviceability
            .borrow()
            .values()
            .find_map(|state| match state {
                Serviceability::ToHeight(height) => Some(*height),
                Serviceability::Absent | Serviceability::Building(_) => None,
            });
        status_of(&self.state.borrow(), tip, target)
    }

    /// Every buffered commit made durable and the record `Stopped`; later
    /// builds are refused.
    async fn shutdown(&self) -> Result<(), ChainStoreError> {
        let mut built = self.built.lock().await;
        built.shut = true;
        let mut failure = None;
        for index in &built.indexes {
            if let Err(error) = index.backend.flush() {
                failure.get_or_insert(ChainStoreError::Backend {
                    message: format!("{} flush at shutdown", index.id.name()),
                    cause: Some(Box::new(error)),
                });
            }
        }
        let built = &mut *built;
        built
            .recorder
            .durable(&self.storage, verified(&built.indexes));
        built.recorder.set(&self.storage, PrimaryState::Stopped);
        failure.map_or(Ok(()), Err)
    }

    fn publish_tips(
        &self,
        tips: impl Iterator<Item = (IndexId, Option<BlockRef>)> + Clone,
        target: Option<Height>,
    ) {
        self.watermark.send_replace(watermark_of(tips.clone()));
        self.serviceability
            .send_replace(serviceability_of(tips, target));
    }
}

/// The record says `Stopped`, each index's durable tip announced last.
impl<St: Storage, Src> Drop for Inner<St, Src> {
    fn drop(&mut self) {
        let built = self.built.get_mut();
        built
            .recorder
            .durable(&self.storage, verified(&built.indexes));
        built.recorder.set(&self.storage, PrimaryState::Stopped);
    }
}

/// A running store: a set of indexes over `St`, built from `Src`. It holds
/// the store as its primary and keeps the record linked readers follow.
/// Cheap to clone: clones share one store, which stops when the last drops.
pub struct ChainStore<St: Storage, Src> {
    inner: Arc<Inner<St, Src>>,
}

impl<St: Storage, Src> Clone for ChainStore<St, Src> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<St: Storage, Src: ChainStoreSource> ChainStore<St, Src> {
    /// Boots the store at `<root>/<network>/`: locks it, resolves the set,
    /// deletes stale directories (`delete_stale`), opens each index (a schema
    /// mismatch is rebuilt, corruption per `on_corruption`), checks each tip
    /// against the validator (D-32: a different block is rebuilt), records
    /// `METADATA`.
    ///
    /// The record says `Booting` while it runs and `Halted` if it fails.
    pub async fn boot(
        config: &ChainStoreServiceConfig,
        storage: St,
        source: Arc<Src>,
        schedule: Arc<[NetworkUpgradeInfo]>,
    ) -> Result<Self, BootError> {
        let inner = Inner::boot(config, storage, source, schedule).await?;
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Builds every enabled index through `target` (D-67, D-96): see
    /// `usage.md`. Single-flight; refused after `shutdown`.
    pub async fn build_to(&self, target: Height) -> Result<(), ChainStoreSourceError>
    where
        St::Backend: 'static,
        <St::Backend as Backend>::Writer: 'static,
    {
        self.inner.build_to(target).await
    }

    /// The enabled set.
    pub fn set(&self) -> &IndexSet {
        self.inner.set()
    }

    /// Watches the store watermark: the lowest tip among the enabled
    /// indexes (empty when any is).
    pub fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.inner.subscribe_watermark()
    }

    /// Watches each index's serviceability.
    pub fn subscribe_serviceability(&self) -> watch::Receiver<BTreeMap<IndexId, Serviceability>> {
        self.inner.subscribe_serviceability()
    }

    /// `id`'s serviceability now.
    pub fn serviceability(&self, id: IndexId) -> Serviceability {
        self.inner.serviceability(id)
    }

    /// What the store answers now (D-81): a capability once each of its
    /// indexes holds a block.
    pub fn capabilities(&self) -> StoreCapabilities {
        self.inner.capabilities()
    }
}

impl<St: Storage, Src: ChainStoreSource> StatusSource for ChainStore<St, Src> {
    fn status(&self) -> ComponentStatus {
        self.inner.status()
    }
}

impl<St, Src> ChainStoreService for ChainStore<St, Src>
where
    St: Storage,
    St::Backend: 'static,
    <St::Backend as Backend>::Reader: Sync + 'static,
    Src: ChainStoreSource,
{
    type Reader = StoreReader<<St::Backend as Backend>::Reader>;

    fn reader(&self) -> Self::Reader {
        self.inner.reader()
    }

    fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.inner.subscribe_watermark()
    }
}

impl<St, Src> ChainStoreIngest for ChainStore<St, Src>
where
    St: Storage,
    St::Backend: 'static,
    <St::Backend as Backend>::Writer: 'static,
    Src: ChainStoreSource,
{
    async fn build_to(&self, target: Height) -> Result<(), ChainStoreSourceError> {
        self.inner.build_to(target).await
    }

    /// Refused (D-97): the store is append-only; a store on the wrong chain
    /// is rebuilt at boot instead (D-32).
    async fn rewind_to(&self, _height: Height) -> Result<(), ChainStoreError> {
        Err(ChainStoreError::Unsupported(
            "rewinding: the store is append-only, rebuilt instead",
        ))
    }

    /// Resolves once no build is running.
    async fn wait_until_built(&self) {
        let _idle = self.inner.built.lock().await;
    }

    async fn shutdown(&self) -> Result<(), ChainStoreError> {
        self.inner.shutdown().await
    }
}

/// Each index's backend, for readers.
fn backends_of<B: Backend>(indexes: &[Open<B>]) -> BTreeMap<IndexId, Arc<B>> {
    indexes
        .iter()
        .map(|index| (index.id, Arc::clone(&index.backend)))
        .collect()
}

/// Each capability whose indexes all hold a block (D-81).
pub(crate) fn capabilities_of(
    serviceability: &BTreeMap<IndexId, Serviceability>,
) -> StoreCapabilities {
    let holds = |id: &IndexId| match serviceability.get(id) {
        Some(Serviceability::ToHeight(_)) | Some(Serviceability::Building(Some(_))) => true,
        Some(Serviceability::Building(None)) | Some(Serviceability::Absent) | None => false,
    };
    StoreCapabilities::new(
        ROOTS
            .iter()
            .filter(|(capability, _)| indexes_of(*capability).all(|id| holds(&id)))
            .map(|(capability, _)| *capability),
    )
}

/// The indexes a capability's port reads directly (D-81). Each pulls in the
/// indexes it declares it reads (`READS`, D-83), so the set a capability needs
/// is derived from the indexes themselves and cannot drift from them.
const ROOTS: &[(StoreCapability, &[IndexId])] = &[
    (StoreCapability::Core, &[IndexId::Headers]),
    (StoreCapability::Transactions, &[IndexId::TxidLocation]),
    (
        StoreCapability::SpentOutputs,
        &[IndexId::Spent, IndexId::TxidLocation, IndexId::Transparent],
    ),
    (StoreCapability::CompactBlocks, &[IndexId::CompactBlock]),
    (StoreCapability::TreeRoots, &[IndexId::ChainMetadata]),
    (StoreCapability::TxOutSet, &[IndexId::TxOutSet]),
    (
        StoreCapability::TransparentHistory,
        &[IndexId::AddressHistory],
    ),
    (StoreCapability::TreeState, &[IndexId::TreeState]),
    (StoreCapability::HistoryTree, &[IndexId::HistoryTree]),
];

/// Every index `capability` needs: its roots and what they read.
pub(crate) fn indexes_of(capability: StoreCapability) -> impl Iterator<Item = IndexId> {
    let roots = ROOTS
        .iter()
        .find(|(root, _)| *root == capability)
        .map_or(&[][..], |(_, ids)| *ids);
    let mut all: Vec<IndexId> = roots
        .iter()
        .flat_map(|id| std::iter::once(*id).chain(id.reads()))
        .collect();
    all.sort_unstable();
    all.dedup();
    all.into_iter()
}

/// An index's tip as served: none while unverified.
fn served_tip<B: Backend>(index: &Open<B>) -> Option<BlockRef> {
    index.tip.filter(|_| !index.unverified)
}

/// The lowest of the indexes' served tips; empty when any is.
pub(crate) fn watermark_of(
    tips: impl Iterator<Item = (IndexId, Option<BlockRef>)>,
) -> StoreWatermark {
    let tip = tips
        .map(|(_, tip)| tip)
        .collect::<Option<Vec<_>>>()
        .and_then(|tips| tips.into_iter().min_by_key(|tip| tip.height));
    StoreWatermark {
        tip,
        provenance: Provenance::Durable,
    }
}

/// Each index built to `target`, or still building.
pub(crate) fn serviceability_of(
    tips: impl Iterator<Item = (IndexId, Option<BlockRef>)>,
    target: Option<Height>,
) -> BTreeMap<IndexId, Serviceability> {
    tips.map(|(id, tip)| {
        let reached = target
            .zip(tip)
            .filter(|(target, tip)| tip.height >= *target)
            .map(|(_, tip)| tip.height);
        let serviceability = match reached {
            Some(height) => Serviceability::ToHeight(height),
            None => Serviceability::Building(tip.map(|tip| tip.height)),
        };
        (id, serviceability)
    })
    .collect()
}

/// The directories index directories live in: the default and any
/// per-index paths.
pub(crate) fn parents(
    config: &ChainStoreServiceConfig,
    root: &Path,
    set: &IndexSet,
) -> BTreeMap<Option<IndexId>, PathBuf> {
    let mut parents = BTreeMap::from([(None, root.join("indexes"))]);
    for id in set.iter() {
        if let Some(path) = config.indexes.paths.get(id.name()) {
            parents.insert(Some(id), path.clone());
        }
    }
    parents
}

pub(crate) fn parent_of(parents: &BTreeMap<Option<IndexId>, PathBuf>, id: IndexId) -> &Path {
    parents
        .get(&Some(id))
        .or_else(|| parents.get(&None))
        .map_or(Path::new("indexes"), PathBuf::as_path)
}

/// Every `<index>_<format>` directory the set does not open, deleted when
/// `delete_stale`; anything else left and logged.
fn sweep<St: Storage>(
    storage: &St,
    parents: &BTreeMap<Option<IndexId>, PathBuf>,
    set: &IndexSet,
    delete_stale: bool,
) -> Result<(), BootError> {
    let mut seen = Vec::new();
    for parent in parents.values() {
        if seen.contains(&parent) {
            continue;
        }
        seen.push(parent);
        let names = storage
            .children(parent)
            .map_err(|source| BootError::Storage {
                index: "store",
                source,
            })?;
        for name in names {
            let Some(id) = index_of_dir(&name) else {
                tracing::warn!(dir = %parent.join(&name).display(), "not an index directory: left");
                continue;
            };
            let opened = set.contains(id)
                && parent_of(parents, id) == parent.as_path()
                && name == id.schema().dir_name();
            if opened {
                continue;
            }
            let dir = parent.join(&name);
            match delete_stale {
                true => {
                    tracing::info!(dir = %dir.display(), "deleting stale index directory");
                    storage.remove(&dir).map_err(|source| BootError::Storage {
                        index: id.name(),
                        source,
                    })?;
                }
                false => {
                    tracing::info!(dir = %dir.display(), "keeping stale index directory (delete_stale = false)")
                }
            }
        }
    }
    Ok(())
}

/// The index a `<name>_<format>` directory belongs to.
fn index_of_dir(name: &str) -> Option<IndexId> {
    let (index, format) = name.rsplit_once('_')?;
    format.parse::<u16>().ok()?;
    IndexId::from_name(index)
}

/// Logs how the set differs from the last boot's (`METADATA` is advisory).
fn report_change<St: Storage>(storage: &St, root: &Path, network: &str, set: &IndexSet) {
    let now = Metadata::of(network, set.iter());
    match storage.read_meta(root, metadata::NAME) {
        Ok(None) => {}
        Ok(Some(bytes)) => match Metadata::decode(&bytes) {
            Ok(last) if last == now => {}
            Ok(last) => {
                tracing::info!(last = ?last.indexes, now = ?now.indexes, "index set changed since the last boot")
            }
            Err(error) => tracing::warn!(%error, "METADATA unreadable: rewriting"),
        },
        Err(error) => tracing::warn!(%error, "METADATA unreadable: rewriting"),
    }
}

/// `id` opened at `dir` and resumed; a schema mismatch is rebuilt,
/// corruption follows `on_corruption`.
fn open<St: Storage>(
    storage: &St,
    dir: &Path,
    id: IndexId,
    network: &str,
    on_corruption: OnCorruption,
    recorder: &mut Recorder<St>,
) -> Result<Open<St::Backend>, BootError> {
    match try_open(storage, dir, id, network) {
        Ok(open) => Ok(open),
        Err(Damage::Mismatch(error)) => {
            tracing::warn!(index = id.name(), %error, "stamped with another layout: rebuilding");
            rebuild(storage, dir, id, network, recorder)
        }
        Err(Damage::Corrupted(error)) => match on_corruption {
            OnCorruption::Rebuild => {
                tracing::warn!(index = id.name(), %error, "corrupted: rebuilding");
                rebuild(storage, dir, id, network, recorder)
            }
            OnCorruption::Halt => Err(BootError::Halted {
                index: id.name(),
                source: error,
            }),
        },
        Err(Damage::Unavailable(source)) => Err(BootError::Storage {
            index: id.name(),
            source,
        }),
    }
}

/// `dir` deleted and opened empty, the record saying so first (its index no
/// longer announced).
fn rebuild<St: Storage>(
    storage: &St,
    dir: &Path,
    id: IndexId,
    network: &str,
    recorder: &mut Recorder<St>,
) -> Result<Open<St::Backend>, BootError> {
    recorder.record.durable.remove(&id);
    recorder.set(storage, PrimaryState::Rebuilding(id));
    storage.remove(dir).map_err(|source| BootError::Storage {
        index: id.name(),
        source,
    })?;
    try_open(storage, dir, id, network).map_err(|damage| BootError::Storage {
        index: id.name(),
        source: damage.into_open_error(),
    })
}

/// What stopped an index opening.
enum Damage {
    Mismatch(OpenError),
    Corrupted(Box<dyn std::error::Error + Send + Sync>),
    Unavailable(OpenError),
}

impl Damage {
    fn into_open_error(self) -> OpenError {
        match self {
            Damage::Mismatch(error) | Damage::Unavailable(error) => error,
            Damage::Corrupted(error) => OpenError::Corrupted(error),
        }
    }
}

/// What `id`'s directory is stamped with on `network`.
pub(crate) fn identity(id: IndexId, network: &str) -> Identity<'_> {
    let schema = id.schema();
    Identity {
        name: schema.name,
        chain: network,
        format: schema.format,
        schema: schema.digest(),
    }
}

fn try_open<St: Storage>(
    storage: &St,
    dir: &Path,
    id: IndexId,
    network: &str,
) -> Result<Open<St::Backend>, Damage> {
    let backend = storage
        .open(dir, identity(id, network), &id.schema().specs())
        .map_err(|error| match error {
            OpenError::Mismatch(_) => Damage::Mismatch(error),
            OpenError::Corrupted(cause) => Damage::Corrupted(cause),
            other => Damage::Unavailable(other),
        })?;
    let reader = backend.reader().map_err(Damage::Unavailable)?;
    let tip = reader.tip().map_err(read_damage)?;
    let state = id.resume(&reader).map_err(|error| match error {
        FoldError::Read(error) => read_damage(error),
        other => Damage::Corrupted(Box::new(other)),
    })?;
    let writer = backend.writer().map_err(Damage::Unavailable)?;
    Ok(Open {
        id,
        dir: dir.to_path_buf(),
        backend: Arc::new(backend),
        writer,
        state,
        tip,
        unverified: false,
    })
}

/// Why a tip check stopped.
enum TipCheck {
    Source(ChainStoreSourceError),
    Rebuild(BootError),
}

/// D-32 over the unverified indexes: each tip the validator has reached is
/// checked against the validator's block at that height, verified on a
/// match and rebuilt otherwise; a tip above the validator's stays
/// unverified (held, not served) until it is reached.
async fn check_tips<St: Storage, Src: ChainStoreSource>(
    storage: &St,
    source: &Src,
    network: &str,
    indexes: &mut [Open<St::Backend>],
    recorder: &mut Recorder<St>,
) -> Result<(), TipCheck> {
    let validator_tip = source
        .get_best_block_height()
        .await
        .map_err(|error| TipCheck::Source(crate::assemble::query("best height", error)))?;
    let mut checked: BTreeMap<Height, BlockHash> = BTreeMap::new();
    for index in indexes.iter_mut().filter(|index| index.unverified) {
        let Some(tip) = index.tip else {
            index.unverified = false;
            continue;
        };
        if tip.height > validator_tip {
            tracing::info!(
                index = index.id.name(),
                height = %tip.height,
                "stored tip above the validator's: held until the validator reaches it"
            );
            continue;
        }
        let theirs = match checked.get(&tip.height) {
            Some(hash) => *hash,
            None => {
                let hash = validator_hash(source, tip.height)
                    .await
                    .map_err(TipCheck::Source)?;
                checked.insert(tip.height, hash);
                hash
            }
        };
        if theirs == tip.hash {
            index.unverified = false;
            continue;
        }
        tracing::warn!(
            index = index.id.name(),
            height = %tip.height,
            "stored tip is not the validator's block: rebuilding"
        );
        let dir = index.dir.clone();
        *index = rebuild(storage, &dir, index.id, network, recorder).map_err(TipCheck::Rebuild)?;
    }
    Ok(())
}

fn read_damage(error: ReadError) -> Damage {
    match error {
        ReadError::Corrupted { .. } => Damage::Corrupted(Box::new(error)),
        other => Damage::Unavailable(OpenError::Unavailable {
            operation: "resume",
            source: Box::new(other),
        }),
    }
}

async fn validator_hash<Src: ChainStoreSource>(
    source: &Src,
    height: Height,
) -> Result<BlockHash, ChainStoreSourceError> {
    source
        .get_block(height)
        .await
        .map(|block| block.header.hash)
        .map_err(|error| crate::assemble::query("tip check block", error))
}

pub(crate) fn fold_error(id: IndexId, error: FoldError) -> ChainStoreSourceError {
    ChainStoreSourceError::InconsistentData {
        message: format!("{} fold", id.name()),
        cause: Some(Box::new(error)),
    }
}

pub(crate) fn commit_error(
    id: IndexId,
    error: impl std::error::Error + Send + Sync + 'static,
) -> ChainStoreSourceError {
    ChainStoreSourceError::Commit {
        message: format!("{} commit", id.name()),
        cause: Some(Box::new(error)),
    }
}
