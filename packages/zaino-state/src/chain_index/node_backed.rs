//! The chain index: the service (ChainView, the chain head, the store and
//! the mempool), held under ChainIndex's legacy surface (D6-09).

use std::sync::Arc;

use crate::legacy::chain::FinalisedStateMode;
use crate::legacy::status::{NamedAtomicStatus, Status, StatusType};
use zaino_chain::{ForkReconcile as _, Locator};
use zaino_chain_store_service::{
    ChainStoreServiceConfig, IndexesConfig, IngestConfig, NamedSet, OnCorruption,
};
use zaino_component::{ComponentStatus, Health, Lifecycle};
use zaino_persistence_lsm_tree::{fs::RealFs, LsmConfig, LsmTreeStorage};
use zaino_service::{DiskStore, Mode, Service, ServiceConfig, Store};

use zaino_service::ServiceSource;

use super::chain_head;
use super::chain_view::ChainIndexSnapshot;
use super::source_ports::ChainIndexSourcePorts;
use super::types;
use super::validator_source::ValidatorSource;
use super::ZebraNetwork;
use crate::error::ChainIndexError;

/// The LSM write buffer per index: `zaino-bench`'s measured default.
const WRITE_BUFFER: std::num::NonZeroUsize = match std::num::NonZeroUsize::new(64 << 20) {
    Some(bytes) => bytes,
    None => std::num::NonZeroUsize::MIN,
};

/// Folds the statuses of the index's components into the index's own.
///
/// Components are read live on every call, so a component that recovers is
/// reported as recovered.
pub(crate) fn combine_component_statuses(
    own: StatusType,
    finalised: StatusType,
    mempool: StatusType,
    coherence: StatusType,
    chain_head: StatusType,
) -> StatusType {
    own.combine(finalised)
        .combine(mempool)
        .combine(coherence)
        .combine(chain_head)
}

/// The index's serving status: its own, folded with the service's
/// components. A store still building serves through the validator, so its
/// build shows in `finalised_state_mode`, the manifest and the metrics, not
/// here.
fn combined_status(
    own: StatusType,
    service: &Service<impl zaino_service::ServiceSource>,
) -> StatusType {
    let statuses = service.statuses();
    let store = statuses
        .sync
        .into_iter()
        .fold(serving(statuses.store), |store, sync| {
            store.combine(serving(sync))
        });
    combine_component_statuses(
        own,
        store,
        fused(statuses.mempool),
        fused(statuses.coherence),
        fused(statuses.head),
    )
}

/// A store component's status as serving health: building is serving.
fn serving(status: ComponentStatus) -> StatusType {
    match (status.health, status.lifecycle) {
        (Health::Healthy, Lifecycle::Spawning | Lifecycle::Syncing) => StatusType::Ready,
        _ => fused(status),
    }
}

/// A component's two axes, back as the one status this index folds.
///
/// Health first, because that is the axis `combine` ranks.
fn fused(status: ComponentStatus) -> StatusType {
    match status.health {
        Health::Critical => StatusType::CriticalError,
        Health::Recoverable => StatusType::RecoverableError,
        Health::Offline => StatusType::Offline,
        Health::Healthy => match status.lifecycle {
            Lifecycle::Offline => StatusType::Offline,
            Lifecycle::Spawning => StatusType::Spawning,
            Lifecycle::Syncing => StatusType::Syncing,
            Lifecycle::Ready => StatusType::Ready,
            Lifecycle::Closing => StatusType::Closing,
        },
    }
}

/// The service's config from the legacy one (D6-13, D6-14): an ephemeral
/// index is validator-backed; a persistent one builds every index but
/// `history_tree` in `storage.database.path`. The legacy store's tuning knobs
/// no longer apply, and setting one is warned.
fn service_config(
    config: &crate::config::ChainIndexConfig,
    chain_head: zaino_chain_head::ChainHeadConfig,
) -> ServiceConfig {
    warn_legacy_knobs(config);
    let network = zaino_primitives::types::Network::from_zebra(&config.network);
    let store = match config.ephemeral {
        true => Store::ValidatorBacked { network },
        false => Store::DiskAccelerated(Box::new(DiskStore {
            store: ChainStoreServiceConfig {
                root: config.storage.database.path.clone(),
                network,
                indexes: IndexesConfig {
                    set: Some(NamedSet::Full),
                    disable: vec!["history_tree".to_owned()],
                    ..IndexesConfig::default()
                },
                on_corruption: OnCorruption::default(),
                ingest: IngestConfig::default(),
            },
            storage: LsmTreeStorage {
                fs: RealFs::shared(),
                lsm: LsmConfig::default(),
                write_buffer: WRITE_BUFFER,
                threads: None,
                strict_manifest_read: false,
            },
            mode: Mode::Primary,
        })),
    };
    ServiceConfig {
        store,
        chain_view: zaino_chain::ChainViewConfig::default(),
        chain_head,
        mempool: config.mempool.clone(),
    }
}

/// Warns for each legacy store knob set away from its default.
fn warn_legacy_knobs(config: &crate::config::ChainIndexConfig) {
    for knob in legacy_knobs_set(config) {
        tracing::warn!(knob, "deprecated: the store no longer reads this setting");
    }
}

/// The legacy store knobs set away from their defaults.
fn legacy_knobs_set(config: &crate::config::ChainIndexConfig) -> Vec<&'static str> {
    let database = &config.storage.database;
    let defaults = crate::legacy::config::DatabaseConfig::default();
    let cache = crate::legacy::config::CacheConfig::default();
    let set = [
        ("storage.database.size", database.size != defaults.size),
        (
            "storage.database.sync_write_batch_size",
            database.sync_write_batch_size != defaults.sync_write_batch_size,
        ),
        (
            "storage.database.accumulator_rebuild_memory_size",
            database.accumulator_rebuild_memory_size != defaults.accumulator_rebuild_memory_size,
        ),
        (
            "storage.database.sync_checkpoint_interval",
            database.sync_checkpoint_interval != defaults.sync_checkpoint_interval,
        ),
        (
            "storage.cache",
            config.storage.cache.capacity != cache.capacity
                || config.storage.cache.shard_power != cache.shard_power,
        ),
    ];
    set.into_iter()
        .filter_map(|(knob, set)| set.then_some(knob))
        .collect()
}

/// The chain index: the service, under ChainIndex's legacy surface.
///
/// # Construction
///
/// Use [`NodeBackedChainIndex::new()`] with:
/// - A [`ValidatorSource`] over a `zaino-source` validator — in production
///   [`ZebraValidatorSource`](crate::chain_index::validator_source::ZebraValidatorSource),
///   built by `spawn_rpc` or `spawn_direct` from the backend config
/// - A [`ChainIndexConfig`](crate::ChainIndexConfig) containing cache and database settings
///
/// Most consumers should not build one directly:
/// [`NodeBackedIndexerService`](crate::NodeBackedIndexerService) does it from
/// config, and additionally waits for the initial sync to complete.
pub struct NodeBackedChainIndex<
    V: ChainIndexSourcePorts + ServiceSource = zaino_source_zebra::ZebraValidator,
> {
    service: Arc<Service<V>>,
    ephemeral: bool,
    status: NamedAtomicStatus,
    network: ZebraNetwork,
    source: ValidatorSource<V>,
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedChainIndex<V> {
    /// Starts the index: the service, which serves at once and builds the
    /// store in the background.
    pub async fn new(
        source: ValidatorSource<V>,
        config: crate::config::ChainIndexConfig,
    ) -> Result<Self, crate::InitError> {
        Self::new_with_chain_head_config(source, config, chain_head::config()).await
    }

    pub(crate) async fn new_with_chain_head_config(
        source: ValidatorSource<V>,
        config: crate::config::ChainIndexConfig,
        chain_head_config: zaino_chain_head::ChainHeadConfig,
    ) -> Result<Self, crate::InitError> {
        let service = Service::spawn(
            source.validator(),
            service_config(&config, chain_head_config),
        )
        .await?;
        Ok(Self {
            service: Arc::new(service),
            ephemeral: config.ephemeral,
            status: NamedAtomicStatus::new("ChainIndex", StatusType::Ready),
            network: config.network.clone(),
            source,
        })
    }

    /// The store's committed tip, for tests that watch the seam advance.
    #[cfg(test)]
    pub(crate) fn finalised_tip(&self) -> Option<zaino_primitives::types::Height> {
        self.service.store_tip()
    }

    /// Whether every index the store builds answers locally, for tests.
    #[cfg(test)]
    pub(crate) fn all_local(&self) -> bool {
        self.service.is_caught_up()
            && !self.service.manifest().iter().any(|(_, provision)| {
                matches!(provision, zaino_service::Provision::Building { .. })
            })
    }

    /// Creates a [`NodeBackedChainIndexSubscriber`] from self,
    /// a clone-safe, drop-safe, read-only view onto the running indexer.
    pub fn subscriber(&self) -> NodeBackedChainIndexSubscriber<V> {
        NodeBackedChainIndexSubscriber {
            service: Arc::clone(&self.service),
            ephemeral: self.ephemeral,
            status: self.status.clone(),
            network: self.network.clone(),
            source: self.source.clone(),
        }
    }

    /// Shut down the index, for a cleaner drop.
    /// An error indicates a failure to cleanly shutdown. Dropping the
    /// chain index should still stop everything.
    pub async fn shutdown(&self) -> Result<(), zaino_chain_store::ChainStoreError> {
        self.status.store(StatusType::Closing);
        let stopped = self.service.shutdown().await;
        zaino_source::SourceLifecycle::shutdown(self.source.source());
        stopped
    }

    /// The synchronous teardown: every task stops; the store skips its flush.
    pub(crate) fn shutdown_sync_best_effort(&self) {
        self.status.store(StatusType::Closing);
        self.service.stop();
        zaino_source::SourceLifecycle::shutdown(self.source.source());
    }

    /// How long tip-coherent mempool reads have been frozen, or `None` if live.
    ///
    /// A brief non-`None` value is the normal shape of a tip transition. A
    /// sustained one means the validator tip and Zaino's have stopped agreeing,
    /// and every tip-coherent read is failing.
    pub fn mempool_coherence_health(&self) -> Option<std::time::Duration> {
        self.service.mempool_frozen_for()
    }

    /// Returns which backend is currently answering finalised-state reads.
    ///
    /// Companion to [`NodeBackedChainIndex::status`], which cannot express this: an ephemeral
    /// passthrough reports [`StatusType::Ready`] identically to a synced persistent database.
    pub fn finalised_state_mode(&self) -> FinalisedStateMode {
        state_mode(self.ephemeral, &self.service)
    }

    /// Displays the status of the chain_index
    pub fn status(&self) -> StatusType {
        combined_status(self.status.load(), &self.service)
    }

    /// Waits until the chain head's window has reached the validator's tip
    /// (its first advance). The store is not waited on: it builds in the
    /// background and ChainView answers from the validator meanwhile.
    pub(crate) async fn wait_for_head(&self) -> Result<(), ChainIndexError> {
        loop {
            let head = self.service.statuses().head;
            match (head.lifecycle, head.health) {
                (Lifecycle::Ready | Lifecycle::Closing, _) => return Ok(()),
                (Lifecycle::Offline, _) | (_, Health::Critical | Health::Offline) => {
                    return Err(ChainIndexError::internal(format!(
                        "the chain head did not start: {head:?}"
                    )))
                }
                _ => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
            }
        }
    }
}

/// Which backend answers finalised-state reads.
fn state_mode(
    ephemeral: bool,
    service: &Service<impl zaino_service::ServiceSource>,
) -> FinalisedStateMode {
    match (ephemeral, service.is_caught_up()) {
        (true, _) => FinalisedStateMode::EphemeralConfigured,
        (false, true) => FinalisedStateMode::Persistent,
        (false, false) => FinalisedStateMode::EphemeralSyncing,
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> Drop for NodeBackedChainIndex<V> {
    /// The full synchronous teardown: subscribers share the service, so its
    /// own drop may come later.
    fn drop(&mut self) {
        self.shutdown_sync_best_effort();
    }
}

/// Formatted by hand: a derive would demand `Debug` of the validator types.
impl<V: ChainIndexSourcePorts + ServiceSource> std::fmt::Debug for NodeBackedChainIndex<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeBackedChainIndex")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

/// A clone-safe *read-only* view onto a running [`NodeBackedChainIndex`].
///
/// [`NodeBackedChainIndexSubscriber`] can safely be cloned and dropped freely.
pub struct NodeBackedChainIndexSubscriber<
    V: ChainIndexSourcePorts + ServiceSource = zaino_source_zebra::ZebraValidator,
> {
    pub(super) service: Arc<Service<V>>,
    ephemeral: bool,
    status: NamedAtomicStatus,
    pub(super) network: ZebraNetwork,
    source: ValidatorSource<V>,
}

/// Cloned by hand: a derive would demand `Clone` of the validator types, which
/// are shared behind `Arc`s here.
impl<V: ChainIndexSourcePorts + ServiceSource> Clone for NodeBackedChainIndexSubscriber<V> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
            ephemeral: self.ephemeral,
            status: self.status.clone(),
            network: self.network.clone(),
            source: self.source.clone(),
        }
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> std::fmt::Debug
    for NodeBackedChainIndexSubscriber<V>
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeBackedChainIndexSubscriber")
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedChainIndexSubscriber<V> {
    pub(crate) fn indexed_tip_stream(&self) -> crate::IndexedTipStream {
        crate::stream::indexed_tip_stream(self.service.subscribe_tip())
    }

    pub(crate) fn source(&self) -> &ValidatorSource<V> {
        &self.source
    }

    /// The service's reads over `snapshot`.
    pub(super) fn reads(
        &self,
        snapshot: &ChainIndexSnapshot<V>,
    ) -> zaino_service::ServiceSnapshot<V> {
        self.service.with_chain(snapshot.clone())
    }

    /// The indexer's mempool subscriber.
    ///
    /// Test-only escape hatch. Production code goes through the `ChainIndex`
    /// mempool API.
    #[cfg(feature = "test_dependencies")]
    pub(crate) fn mempool_subscriber(&self) -> zaino_mempool_service::MempoolSubscriber {
        self.service.mempool_subscriber()
    }

    /// How long tip-coherent mempool reads have been frozen, or `None` if live.
    ///
    /// A brief non-`None` value is the normal shape of a tip transition. A
    /// sustained one means the validator tip and Zaino's have stopped agreeing,
    /// and every tip-coherent read is failing.
    pub fn mempool_coherence_health(&self) -> Option<std::time::Duration> {
        self.service.mempool_frozen_for()
    }

    /// Height of the finalised store's committed tip; `None` while the store
    /// is empty or absent (ephemeral mode).
    pub fn finalised_tip_height(&self) -> Option<u32> {
        self.service.store_tip().map(u32::from)
    }

    /// Which backend answers finalised-state reads.
    pub fn finalised_state_mode(&self) -> FinalisedStateMode {
        state_mode(self.ephemeral, &self.service)
    }

    /// Returns the combined status of all chain index components.
    pub fn combined_status(&self) -> StatusType {
        combined_status(self.status.load(), &self.service)
    }

    /// Whether the chain head (on any branch) or the store holds `hash`.
    pub(super) async fn block_hash_known(
        snapshot: &ChainIndexSnapshot<V>,
        hash: types::BlockHash,
    ) -> Result<bool, ChainIndexError> {
        Ok(snapshot
            .fork_point(&Locator::new(vec![types::domain_hash(hash)]))
            .await?
            .is_some())
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> Status for NodeBackedChainIndexSubscriber<V> {
    fn status(&self) -> StatusType {
        self.combined_status()
    }
}

#[cfg(test)]
mod tests {
    use super::{legacy_knobs_set, service_config, serving};
    use crate::config::ChainIndexConfig;
    use crate::legacy::config::{CacheConfig, DatabaseConfig, StorageConfig};
    use crate::legacy::status::StatusType;
    use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle};
    use zaino_service::Store;

    /// A store still building serves (through the validator): its build is
    /// not the index's health; a failing one is.
    #[test]
    fn a_building_store_is_serving() {
        let store = |lifecycle, health| {
            serving(ComponentStatus::new(
                ComponentName("chain store"),
                lifecycle,
                health,
            ))
        };
        assert_eq!(
            store(Lifecycle::Syncing, Health::Healthy),
            StatusType::Ready
        );
        assert_eq!(
            store(Lifecycle::Spawning, Health::Healthy),
            StatusType::Ready
        );
        assert_eq!(
            store(Lifecycle::Syncing, Health::Recoverable),
            StatusType::RecoverableError
        );
        assert_eq!(
            store(Lifecycle::Closing, Health::Healthy),
            StatusType::Closing
        );
    }

    fn config(ephemeral: bool, storage: StorageConfig) -> ChainIndexConfig {
        ChainIndexConfig {
            storage,
            network: zebra_chain::parameters::Network::new_regtest(Default::default()),
            ephemeral,
            mempool: Default::default(),
        }
    }

    /// Only knobs set away from their defaults are warned (D6-14).
    #[test]
    fn only_changed_legacy_knobs_are_warned() {
        assert!(legacy_knobs_set(&config(false, StorageConfig::default())).is_empty());
        let mut storage = StorageConfig::default();
        storage.database.size = crate::legacy::config::DatabaseSize(1);
        storage.cache = CacheConfig {
            capacity: CacheConfig::default().capacity + 1,
            ..CacheConfig::default()
        };
        assert_eq!(
            legacy_knobs_set(&config(false, storage)),
            vec!["storage.database.size", "storage.cache"]
        );
    }

    /// Ephemeral is validator-backed; persistent builds every index but
    /// `history_tree` at the database path (D6-13).
    #[test]
    fn the_legacy_modes_map_onto_stores() {
        let head = crate::chain_index::chain_head::config();
        let ephemeral = service_config(&config(true, StorageConfig::default()), head.clone());
        assert!(matches!(ephemeral.store, Store::ValidatorBacked { .. }));
        let storage = StorageConfig {
            database: DatabaseConfig {
                path: "/somewhere".into(),
                ..DatabaseConfig::default()
            },
            ..StorageConfig::default()
        };
        let Store::DiskAccelerated(disk) = service_config(&config(false, storage), head).store
        else {
            panic!("a persistent index has a store");
        };
        assert_eq!(disk.store.root, std::path::PathBuf::from("/somewhere"));
        assert_eq!(
            disk.store.indexes.set,
            Some(zaino_chain_store_service::NamedSet::Full)
        );
        assert_eq!(disk.store.indexes.disable, vec!["history_tree".to_owned()]);
        assert!(matches!(disk.mode, zaino_service::Mode::Primary));
    }
}
