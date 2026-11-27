//! [`LinkedChainStore`]: a store another process's primary builds, followed
//! read-only (§5.2a, D-95). Nothing is locked, created or removed; each
//! refresh reads the primary's record and moves every index to the durable
//! tip it announces.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;
use zaino_chain_store::{ChainStoreService, StoreCapabilities, StoreWatermark};
use zaino_component::{ComponentStatus, Health, Lifecycle, StatusSource};
use zaino_indexes::IndexId;
use zaino_persistence::{OpenError, ReadOnlyBackend, RefreshError, Storage};
use zaino_primitives::types::{BlockRef, Height, Network};

use crate::config::{network_name, ChainStoreServiceConfig};
use crate::reader::StoreReader;
use crate::record::{status_of, PrimaryRecord, PrimaryState, RecordError};
use crate::set::{IndexSet, SetError};
use crate::store::{
    capabilities_of, holding_of, identity, parent_of, parents, serviceability_of, specs,
    watermark_of, Serviceability,
};

/// The store's primary, as a linked reader last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PrimaryStatus {
    /// A primary holds the store (it releases it however it ends).
    pub running: bool,
    /// Its last record's state; `None` when no primary has written one.
    pub state: Option<PrimaryState>,
    /// The height it last built toward.
    pub target: Option<Height>,
}

/// Why a linked store did not open or refresh.
#[derive(Debug, thiserror::Error)]
pub enum LinkedError {
    /// The index set is malformed.
    #[error("index set")]
    Set(#[from] SetError),
    /// The store's files could not be read.
    #[error("{index}: storage")]
    Storage {
        /// The index or `"store"`.
        index: &'static str,
        /// Why.
        #[source]
        source: OpenError,
    },
    /// The primary's record is unreadable (another version's, or damaged).
    #[error("primary record")]
    Record(#[from] RecordError),
    /// An index could not follow its announcement (another layout or
    /// chain, lost or corrupted data); it is not served.
    #[error("{index}: refresh")]
    Index {
        /// The index.
        index: &'static str,
        /// Why.
        #[source]
        source: RefreshError,
    },
}

struct Linked<St: Storage> {
    storage: St,
    root: PathBuf,
    network: Network,
    set: IndexSet,
    indexes: Vec<(IndexId, St::ReadOnly)>,
    /// Each served index's tip; absent = not served.
    served: Mutex<BTreeMap<IndexId, Option<BlockRef>>>,
    watermark: watch::Sender<StoreWatermark>,
    serviceability: watch::Sender<BTreeMap<IndexId, Serviceability>>,
    primary: watch::Sender<PrimaryStatus>,
    /// Why the last refresh failed; `None` once one succeeds.
    refresh_failure: watch::Sender<Option<String>>,
    /// Why polling stopped for good; `None` while it runs.
    halted: watch::Sender<Option<String>>,
}

/// Failed refreshes in a row before polling stops and the store is critical.
const MAX_REFRESH_FAILURES: u32 = 20;

/// A store built by another process's primary, read-only.
pub struct LinkedChainStore<St: Storage> {
    inner: Arc<Linked<St>>,
}

impl<St: Storage> LinkedChainStore<St> {
    /// The store at `<root>/<network>/` with `config`'s index set, linked:
    /// nothing read until the first [`refresh`](Self::refresh).
    pub fn open(config: &ChainStoreServiceConfig, storage: St) -> Result<Self, LinkedError> {
        let network = network_name(config.network);
        let root = config.root.join(network);
        let set = IndexSet::resolve(
            config.indexes.set,
            &config.indexes.enable,
            &config.indexes.disable,
        )?;
        let parents = parents(config, &root, &set);
        let mut indexes = Vec::new();
        for id in set.build_order() {
            let dir = parent_of(&parents, id).join(id.schema().dir_name());
            let backend = identity(id, network)
                .and_then(|identity| storage.open_read_only(&dir, identity, &specs(id)?))
                .map_err(|source| LinkedError::Storage {
                    index: id.name(),
                    source,
                })?;
            indexes.push((id, backend));
        }
        let absent = indexes
            .iter()
            .map(|(id, _)| (*id, Serviceability::Absent))
            .collect();
        Ok(Self {
            inner: Arc::new(Linked {
                storage,
                root,
                network: config.network,
                set,
                indexes,
                served: Mutex::new(BTreeMap::new()),
                watermark: watch::channel(StoreWatermark::empty()).0,
                serviceability: watch::channel(absent).0,
                primary: watch::channel(PrimaryStatus::default()).0,
                refresh_failure: watch::channel(None).0,
                halted: watch::channel(None).0,
            }),
        })
    }

    /// Reads the primary's record and moves each index to its announced
    /// durable tip; an index the primary does not announce (rebuilding,
    /// unverified, not in its set) is not served. A tip the primary has
    /// already moved past keeps the last state until the next refresh. An
    /// index that cannot follow is not served and reported, after the rest
    /// are refreshed.
    pub fn refresh(&self) -> Result<(), LinkedError> {
        self.inner.refresh_recorded()
    }

    /// Refreshes every `interval` on the runtime's blocking pool until the
    /// returned handle stops or drops; a failed refresh is logged, shown in
    /// the status, and retried, [`MAX_REFRESH_FAILURES`] times in a row at
    /// most: then, as when the task dies, the store is critical.
    pub fn poll(&self, interval: Duration) -> Polling
    where
        St::ReadOnly: 'static,
    {
        let inner = Arc::clone(&self.inner);
        let halted = Arc::clone(&self.inner);
        let run = async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut failures = 0u32;
            loop {
                ticker.tick().await;
                let linked = Arc::clone(&inner);
                let failure =
                    match tokio::task::spawn_blocking(move || linked.refresh_recorded()).await {
                        Ok(Ok(())) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(error) => {
                            let failure = format!("refresh panicked: {error}");
                            inner.refresh_failure.send_replace(Some(failure.clone()));
                            Some(failure)
                        }
                    };
                let Some(failure) = failure else {
                    failures = 0;
                    continue;
                };
                failures = failures.saturating_add(1);
                tracing::warn!(error = %failure, attempt = failures, "linked store refresh failed");
                if failures >= MAX_REFRESH_FAILURES {
                    inner.halted.send_replace(Some(format!(
                        "refresh failed {failures} times in a row: {failure}"
                    )));
                    return;
                }
            }
        };
        // stopped by abort alone, so any return is a failure
        let task = tokio::spawn(zaino_component::supervised(
            zaino_component::CancellationToken::new(),
            run,
            move |reason| {
                halted.halted.send_if_modified(|halted| match halted {
                    Some(_) => false,
                    None => {
                        *halted = Some(format!("polling stopped: {reason}"));
                        true
                    }
                });
            },
        ));
        Polling { task }
    }

    /// The configured set.
    pub fn set(&self) -> &IndexSet {
        &self.inner.set
    }

    /// Watches the store watermark: the lowest served tip (empty when any
    /// index is).
    pub fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.inner.watermark.subscribe()
    }

    /// Watches each index's serviceability.
    pub fn subscribe_serviceability(&self) -> watch::Receiver<BTreeMap<IndexId, Serviceability>> {
        self.inner.serviceability.subscribe()
    }

    /// `id`'s serviceability now.
    pub fn serviceability(&self, id: IndexId) -> Serviceability {
        self.inner
            .serviceability
            .borrow()
            .get(&id)
            .copied()
            .unwrap_or(Serviceability::Absent)
    }

    /// What the store answers now (D-81).
    pub fn capabilities(&self) -> StoreCapabilities {
        capabilities_of(&self.inner.serviceability.borrow())
    }

    /// Watches the primary's status.
    pub fn subscribe_primary(&self) -> watch::Receiver<PrimaryStatus> {
        self.inner.primary.subscribe()
    }

    /// The primary's status at the last refresh.
    pub fn primary(&self) -> PrimaryStatus {
        self.inner.primary.borrow().clone()
    }

    /// A reader over every served index at the last refresh (D-97), taken
    /// under the refresh's lock so its watermark and readers agree.
    pub fn reader(&self) -> StoreReader<<St::ReadOnly as ReadOnlyBackend>::Reader> {
        let inner = &self.inner;
        let served = inner
            .served
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let watermark = *inner.watermark.borrow();
        let (capabilities, holding) = {
            let serviceability = inner.serviceability.borrow();
            (
                capabilities_of(&serviceability),
                holding_of(&serviceability),
            )
        };
        let readers = inner
            .indexes
            .iter()
            .filter(|(id, _)| served.contains_key(id))
            .filter_map(|(id, backend)| match backend.reader() {
                Ok(reader) => Some((*id, reader)),
                Err(error) => {
                    tracing::warn!(index = id.name(), %error, "no reader: its reads are unavailable");
                    None
                }
            })
            .collect();
        let status = self.status();
        StoreReader::new(
            readers,
            watermark,
            capabilities,
            holding,
            status,
            inner.network,
        )
    }
}

impl<St: Storage> Clone for LinkedChainStore<St> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

/// A linked store's status: its primary's state, offline when no primary
/// is running.
impl<St: Storage> StatusSource for LinkedChainStore<St> {
    fn status(&self) -> ComponentStatus {
        let primary = self.inner.primary.borrow().clone();
        let tip = self.inner.watermark.borrow().tip.map(|tip| tip.height);
        let state = match (primary.running, primary.state) {
            (true, Some(state)) => state,
            (true, None) => PrimaryState::Booting,
            (false, Some(PrimaryState::Halted(message))) => PrimaryState::Halted(message),
            (false, _) => PrimaryState::Stopped,
        };
        let mut status = status_of(&state, tip, primary.target);
        if let Some(failure) = self.inner.refresh_failure.borrow().clone() {
            if status.health == Health::Healthy {
                status.health = Health::Recoverable;
            }
            status.reason = Some(format!("refresh failed: {failure}"));
        }
        if let Some(halted) = self.inner.halted.borrow().clone() {
            status.lifecycle = Lifecycle::Offline;
            status.health = Health::Critical;
            status.reason = Some(halted);
        }
        status
    }
}

impl<St> ChainStoreService for LinkedChainStore<St>
where
    St: Storage,
    St::ReadOnly: 'static,
    <St::ReadOnly as ReadOnlyBackend>::Reader: Sync + 'static,
{
    type Reader = StoreReader<<St::ReadOnly as ReadOnlyBackend>::Reader>;

    fn reader(&self) -> Self::Reader {
        LinkedChainStore::reader(self)
    }

    fn subscribe_watermark(&self) -> watch::Receiver<StoreWatermark> {
        self.inner.watermark.subscribe()
    }
}

impl<St: Storage> Linked<St> {
    /// [`refresh`](Self::refresh), its outcome recorded for the status.
    fn refresh_recorded(&self) -> Result<(), LinkedError> {
        let result = self.refresh();
        self.refresh_failure
            .send_replace(result.as_ref().err().map(ToString::to_string));
        result
    }

    fn refresh(&self) -> Result<(), LinkedError> {
        let store = |source| LinkedError::Storage {
            index: "store",
            source,
        };
        let running = self.storage.primary_running(&self.root).map_err(store)?;
        let record = self
            .storage
            .read_record(&self.root)
            .map_err(store)?
            .map(|bytes| PrimaryRecord::decode(&bytes))
            .transpose()?;

        let mut served = self.served.lock().map_err(|_| LinkedError::Storage {
            index: "store",
            source: OpenError::Unavailable {
                operation: "refresh",
                source: "linked state lock poisoned".into(),
            },
        })?;
        let mut failure = None;
        for (id, backend) in &self.indexes {
            let announced = record
                .as_ref()
                .and_then(|record| record.durable.get(id).copied());
            let tip = match announced {
                None => None,
                Some(tip) => match backend.refresh(tip) {
                    Ok(()) => Some(tip),
                    Err(RefreshError::Superseded) => served.get(id).copied(),
                    Err(RefreshError::Absent) => None,
                    Err(source) => {
                        failure.get_or_insert(LinkedError::Index {
                            index: id.name(),
                            source,
                        });
                        None
                    }
                },
            };
            match tip {
                Some(tip) => served.insert(*id, tip),
                None => served.remove(id),
            };
        }

        let target = record.as_ref().and_then(|record| record.target);
        let tips = || {
            self.indexes
                .iter()
                .map(|(id, _)| (*id, served.get(id).copied().flatten()))
        };
        let mut serviceability = serviceability_of(tips(), target);
        for (id, state) in serviceability.iter_mut() {
            if !served.contains_key(id) {
                *state = Serviceability::Absent;
            }
        }
        let watermark = match served.len() == self.indexes.len() {
            true => watermark_of(tips()),
            false => StoreWatermark::empty(),
        };
        self.watermark.send_replace(watermark);
        self.serviceability.send_replace(serviceability);
        self.primary.send_replace(PrimaryStatus {
            running,
            state: record.map(|record| record.state),
            target,
        });
        failure.map_or(Ok(()), Err)
    }
}

/// The poll task of [`LinkedChainStore::poll`]; dropping it stops polling.
pub struct Polling {
    task: JoinHandle<()>,
}

impl Polling {
    /// Stops polling.
    pub fn stop(&self) {
        self.task.abort();
    }
}

impl Drop for Polling {
    fn drop(&mut self) {
        self.task.abort();
    }
}
