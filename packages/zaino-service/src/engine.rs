//! The engine: the store, the chain head with its extensions, ChainView, the
//! background sync and the mempool, spawned and held together.

pub(crate) mod mempool;
pub(crate) mod store;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;
use zaino_chain::{ChainView as _, ChainViewComposer, ChainViewSync, StoreIndexes};
use zaino_chain_head_service::{ChainHeadInitError, ChainHeadService, ChainHeadSubscriber};
use zaino_chain_store::{ChainStoreError, ChainStoreIngest as _, Provenance};
use zaino_chain_store_service::{BootError, ChainStore, LinkedChainStore, LinkedError, Polling};
use zaino_component::{ComponentStatus, Lifecycle, StatusSource as _};
use zaino_indexes::IndexId;
use zaino_primitives::types::{ChainStateEpoch, Height, Network, NetworkUpgradeInfo};

use crate::capability::ServiceManifest;
use crate::config::{DiskStore, Mode, ServiceConfig, Store};
use crate::error::ServiceError;
use crate::reads::ChainSnapshot;
use crate::source::ServiceSource;
use mempool::{Coherence, Mempool};
use store::EngineStore;

/// The chain view the service reads through.
pub(crate) type View<S> = ChainViewComposer<EngineStore<S>, ChainHeadSubscriber, S>;

/// Why the service did not start.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The validator did not report its upgrade schedule.
    #[error("the validator's upgrade schedule: {0}")]
    Schedule(#[source] ServiceError),
    /// The store did not boot.
    #[error(transparent)]
    Boot(#[from] BootError),
    /// The primary's store could not be opened for reading.
    #[error(transparent)]
    Linked(#[from] LinkedError),
    /// The linked store's open did not finish.
    #[error("opening the linked store: {0}")]
    Join(String),
    /// The chain head could not anchor.
    #[error(transparent)]
    ChainHead(#[from] ChainHeadInitError),
}

/// The service's components' statuses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statuses {
    /// The store (or, without one, a stand-in that is always ready).
    pub store: ComponentStatus,
    /// The chain head.
    pub head: ComponentStatus,
    /// The tip-agnostic mempool.
    pub mempool: ComponentStatus,
    /// The mempool's coherence layer.
    pub coherence: ComponentStatus,
    /// A primary store's background sync.
    pub sync: Option<ComponentStatus>,
}

impl Statuses {
    /// Every component's status.
    pub fn all(&self) -> impl Iterator<Item = &ComponentStatus> {
        [&self.store, &self.head, &self.mempool, &self.coherence]
            .into_iter()
            .chain(self.sync.as_ref())
    }
}

/// One running indexer service (D-33, D-48).
pub struct Service<S: ServiceSource> {
    pub(crate) view: Arc<View<S>>,
    store: EngineStore<S>,
    head: Arc<ChainHeadService<S>>,
    sync: Option<Arc<ChainViewSync>>,
    linked: Option<Polling>,
    pub(crate) mempool: Arc<Mempool<S>>,
    pub(crate) coherence: Arc<Coherence>,
    /// Held so streams can borrow it for the service's lifetime.
    coherent: zaino_mempool_service::CoherentSubscriber,
    pub(crate) source: Arc<S>,
    network: Network,
    cancel: CancellationToken,
}

impl<S: ServiceSource> Service<S> {
    /// Starts the service (D6-10): boots or opens the store, anchors the
    /// chain head, starts the mempool and, for a primary store, the
    /// background sync. Returns once serving can begin, never waiting for
    /// the store to catch up: what is not built yet is served from the
    /// validator meanwhile and the manifest says so (6.4).
    pub async fn spawn(source: Arc<S>, config: ServiceConfig) -> Result<Self, SpawnError> {
        let cancel = CancellationToken::new();
        let network = config.network();
        let mut chain_view = config.chain_view.clone();
        let (store, primary, linked, schedule) = match &config.store {
            Store::ValidatorBacked { .. } => {
                chain_view.store_enabled = false;
                (EngineStore::none(network), None, None, None)
            }
            Store::DiskAccelerated(disk) => {
                let DiskStore {
                    store,
                    storage,
                    mode,
                } = disk.as_ref();
                let schedule = schedule(source.as_ref()).await?;
                match mode {
                    Mode::Primary => {
                        let primary = ChainStore::boot(
                            store,
                            storage.clone(),
                            Arc::clone(&source),
                            Arc::clone(&schedule),
                        )
                        .await?;
                        (
                            EngineStore::Primary(primary.clone()),
                            Some(primary),
                            None,
                            Some(schedule),
                        )
                    }
                    Mode::Linked { refresh_interval } => {
                        let (store, storage) = (store.clone(), storage.clone());
                        let linked = tokio::task::spawn_blocking(move || {
                            LinkedChainStore::open(&store, storage)
                        })
                        .await
                        .map_err(|error| SpawnError::Join(error.to_string()))??;
                        let polling = linked.poll(*refresh_interval);
                        (
                            EngineStore::Linked(linked),
                            None,
                            Some(polling),
                            Some(schedule),
                        )
                    }
                }
            }
        };

        let head = spawn_head(&source, &config, &store, schedule, &cancel).await?;
        let (mempool, coherence) =
            mempool::spawn(&source, head.subscriber(), &config.mempool, &cancel);
        let view = Arc::new(ChainViewComposer::new(
            store.clone(),
            head.subscriber(),
            Arc::clone(&source),
            chain_view.clone(),
        ));
        // Built on its own composer over the concrete store: only a primary
        // store can be built, and only it has `spawn_sync`.
        let sync = primary.map(|primary| {
            Arc::new(ChainViewComposer::new(
                primary,
                head.subscriber(),
                Arc::clone(&source),
                chain_view,
            ))
            .spawn_sync(cancel.child_token())
        });

        Ok(Self {
            view,
            store,
            head,
            sync,
            linked,
            mempool,
            coherent: coherence.subscriber(),
            coherence,
            source,
            network,
            cancel,
        })
    }

    /// The network served.
    pub fn network(&self) -> Network {
        self.network
    }

    /// The chain view's snapshot now.
    pub fn chain_snapshot(&self) -> ChainSnapshot<S> {
        self.view.snapshot()
    }

    /// Epochs as the chain head publishes them.
    pub fn subscribe_tip(&self) -> tokio::sync::watch::Receiver<ChainStateEpoch> {
        self.view.subscribe_tip()
    }

    /// The store's committed tip, `None` while empty or without a store.
    pub fn store_tip(&self) -> Option<Height> {
        let watermark = zaino_chain_store::ChainStoreReader::watermark(
            &zaino_chain_store::ChainStoreService::reader(&self.store),
        );
        match watermark.provenance {
            Provenance::Durable => watermark.tip.map(|tip| tip.height),
            Provenance::Passthrough => None,
        }
    }

    /// How long the coherent mempool has been frozen, `None` while live.
    pub fn mempool_frozen_for(&self) -> Option<std::time::Duration> {
        self.coherent.frozen_for()
    }

    /// Resolves once a primary store has caught up with the chain head's
    /// floor; at once without one. `Err` if the sync stops first.
    pub async fn caught_up(&self) -> Result<(), ServiceError> {
        let Some(sync) = &self.sync else {
            return Ok(());
        };
        let mut status = zaino_component::StatusWatch::subscribe(sync.as_ref());
        let caught_up = status
            .wait_for(|status| status.lifecycle == Lifecycle::Ready || !status.is_live())
            .await
            .map(|status| status.lifecycle == Lifecycle::Ready);
        match caught_up {
            Ok(true) => Ok(()),
            Ok(false) | Err(_) => Err(ServiceError::SyncStopped),
        }
    }

    /// Who answers each capability now.
    pub fn manifest(&self) -> ServiceManifest {
        let store = &self.store;
        ServiceManifest::derive(&self.view.serviceability(), |id| store.serviceability(id))
    }

    /// Each component's status.
    pub fn statuses(&self) -> Statuses {
        Statuses {
            store: self.store.status(),
            head: self.head.status(),
            mempool: self.mempool.status(),
            coherence: self.coherence.status(),
            sync: self.sync.as_ref().map(|sync| sync.status()),
        }
    }

    /// Whether a primary store has caught up with the chain head's floor;
    /// true without one.
    pub fn is_caught_up(&self) -> bool {
        self.sync
            .as_ref()
            .is_none_or(|sync| sync.status().lifecycle == Lifecycle::Ready)
    }

    /// The tip-agnostic mempool set.
    pub fn mempool_subscriber(&self) -> zaino_mempool_service::MempoolSubscriber {
        self.mempool.subscriber()
    }

    /// The mempool's txids, whatever the tip.
    pub fn mempool_txids(&self) -> Arc<[zaino_primitives::types::TransactionId]> {
        self.mempool.subscriber().get_txids()
    }

    /// The mempool's size and cost, whatever the tip.
    pub fn mempool_info(&self) -> zaino_mempool_service::MempoolInfo {
        self.mempool.subscriber().get_mempool_info()
    }

    /// The mempool's entries but those whose txid ends in one of `exclude`;
    /// a malformed suffix is refused, never trimmed.
    pub fn mempool_entries(
        &self,
        exclude: &[Vec<u8>],
    ) -> Result<Vec<Arc<zaino_mempool::MempoolEntry>>, ServiceError> {
        crate::mempool_reads::entries(&self.mempool.subscriber(), exclude)
    }

    /// The coherent mempool's stream until the tip moves; with `chain`, only
    /// if coherent with that snapshot (`None` otherwise).
    pub fn mempool_stream(
        &self,
        chain: Option<&ChainSnapshot<S>>,
    ) -> Option<impl futures::Stream<Item = Result<bytes::Bytes, ServiceError>> + Send + '_> {
        crate::mempool_reads::coherent_stream(
            &self.coherent,
            chain.map(zaino_chain::ChainViewSnapshot::epoch),
        )
    }

    /// Stops every task; a primary store flushes. Dropping the service stops
    /// the tasks too, without the flush.
    pub async fn shutdown(&self) -> Result<(), ChainStoreError> {
        self.stop();
        match &self.store {
            EngineStore::Primary(store) => store.shutdown().await,
            EngineStore::Linked(_) | EngineStore::None(_) => Ok(()),
        }
    }

    /// Stops every task without the flush; for synchronous teardown.
    pub fn stop(&self) {
        self.cancel.cancel();
        self.mempool.close();
        self.coherence.close();
        self.head.shutdown();
        if let Some(sync) = &self.sync {
            sync.shutdown();
        }
        if let Some(linked) = &self.linked {
            linked.stop();
        }
        zaino_source::SourceLifecycle::shutdown(self.source.as_ref());
    }
}

impl<S: ServiceSource> Drop for Service<S> {
    fn drop(&mut self) {
        self.stop();
    }
}

impl<S: ServiceSource> std::fmt::Debug for Service<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// The validator's upgrade schedule, which the store and the head's
/// extensions fold against.
async fn schedule<S: ServiceSource>(source: &S) -> Result<Arc<[NetworkUpgradeInfo]>, SpawnError> {
    let info = zaino_source::OneShotGetBlockchainInfo::get_blockchain_info(source)
        .await
        .map_err(|error| SpawnError::Schedule(ServiceError::from_query(error)))?;
    Ok(Arc::from(info.upgrades))
}

/// The chain head, with an extension for each index the store builds: each
/// folds that index over the window from the store's tip (D-50).
async fn spawn_head<S: ServiceSource>(
    source: &Arc<S>,
    config: &ServiceConfig,
    store: &EngineStore<S>,
    schedule: Option<Arc<[NetworkUpgradeInfo]>>,
    cancel: &CancellationToken,
) -> Result<Arc<ChainHeadService<S>>, ChainHeadInitError> {
    let builder = ChainHeadService::builder(
        Arc::clone(source),
        config.chain_head.clone(),
        cancel.child_token(),
    );
    let (Some(set), Some(schedule)) = (store.set(), schedule) else {
        return builder.spawn().await;
    };
    let indexes = Arc::new(StoreIndexes::new(store.clone()));
    let mut head = builder.extensions(schedule, Some(indexes.clone()));
    if set.contains(IndexId::TxidLocation) || set.contains(IndexId::Spent) {
        head = head.with_transactions();
    }
    if set.contains(IndexId::TxOutSet) {
        head = head.with_txout_set(Arc::clone(&indexes));
    }
    if set.contains(IndexId::AddressHistory) {
        head = head.with_address_history(Arc::clone(&indexes));
    }
    if set.contains(IndexId::CompactBlock) {
        head = head.with_compact_blocks(Arc::clone(&indexes));
    }
    if set.contains(IndexId::TreeState) {
        head = head.with_tree_state(Arc::clone(&indexes));
    }
    if set.contains(IndexId::HistoryTree) {
        head = head.with_history_tree(indexes);
    }
    head.spawn().await
}
