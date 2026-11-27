//! The store behind the service: built here, followed from another process,
//! or none.

use std::sync::Arc;

use tokio::sync::watch;
use zaino_chain_store::{ChainStoreService, StoreWatermark};
use zaino_chain_store_service::{
    ChainStore, IndexSet, LinkedChainStore, Serviceability, StoreReader,
};
use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle, StatusSource};
use zaino_indexes::IndexId;
use zaino_persistence_lsm_tree::{LsmTreeReader, LsmTreeStorage};
use zaino_primitives::types::Network;

use crate::source::ServiceSource;

/// The service's store, one type whichever mode runs.
pub(crate) enum EngineStore<S> {
    /// Booted and built by this process.
    Primary(ChainStore<LsmTreeStorage, S>),
    /// Built by another process; read here.
    Linked(LinkedChainStore<LsmTreeStorage>),
    /// No store: the validator backs the service.
    None(Arc<NoStore>),
}

/// A validator-backed deployment's store: holds nothing.
pub(crate) struct NoStore {
    reader: StoreReader<LsmTreeReader>,
    watermark: watch::Sender<StoreWatermark>,
}

impl<S> EngineStore<S> {
    pub(crate) fn none(network: Network) -> Self {
        let reader = StoreReader::empty(network, no_store_status());
        let (watermark, _) =
            watch::channel(zaino_chain_store::ChainStoreReader::watermark(&reader));
        Self::None(Arc::new(NoStore { reader, watermark }))
    }
}

fn no_store_status() -> ComponentStatus {
    ComponentStatus::new(
        ComponentName("chain_store"),
        Lifecycle::Ready,
        Health::Healthy,
    )
}

impl<S: ServiceSource> EngineStore<S> {
    /// The index set, empty without a store.
    pub(crate) fn set(&self) -> Option<&IndexSet> {
        match self {
            Self::Primary(store) => Some(store.set()),
            Self::Linked(store) => Some(store.set()),
            Self::None(_) => None,
        }
    }

    /// How far `id` serves.
    pub(crate) fn serviceability(&self, id: IndexId) -> Serviceability {
        match self {
            Self::Primary(store) => store.serviceability(id),
            Self::Linked(store) => store.serviceability(id),
            Self::None(_) => Serviceability::Absent,
        }
    }
}

impl<S> Clone for EngineStore<S> {
    fn clone(&self) -> Self {
        match self {
            Self::Primary(store) => Self::Primary(store.clone()),
            Self::Linked(store) => Self::Linked(store.clone()),
            Self::None(store) => Self::None(Arc::clone(store)),
        }
    }
}

impl<S: ServiceSource> StatusSource for EngineStore<S> {
    fn status(&self) -> ComponentStatus {
        match self {
            Self::Primary(store) => store.status(),
            Self::Linked(store) => store.status(),
            Self::None(store) => store.reader.status(),
        }
    }
}

impl<S: ServiceSource> ChainStoreService for EngineStore<S> {
    type Reader = StoreReader<LsmTreeReader>;

    fn reader(&self) -> Self::Reader {
        match self {
            Self::Primary(store) => store.reader(),
            Self::Linked(store) => store.reader(),
            Self::None(store) => store.reader.clone(),
        }
    }

    fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        match self {
            Self::Primary(store) => store.subscribe_watermark(),
            Self::Linked(store) => store.subscribe_watermark(),
            Self::None(store) => store.watermark.subscribe(),
        }
    }
}
