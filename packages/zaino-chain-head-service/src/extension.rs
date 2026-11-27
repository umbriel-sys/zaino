//! The head's extensions (D-50): one `zaino-indexes` fold each, run over the
//! window above a durable root. Every retained block above the root carries
//! the index after it (its fold state and an overlay of the rows the window
//! wrote), so a view costs one fold per block, branches need no extra work,
//! and an answer is a typed read over the tip's view. Answers cover the window
//! alone; what lies below the root is asked through the ports in
//! `zaino_chain_head::extension`, and seeded from them at the root.
//!
//! The root is the store's tip (D-53), asked every tick; the views are
//! rebased onto it once it has advanced [`REBASE_EVERY`] blocks. A failing
//! extension darkens its own slot and is rebuilt at the next tick, or as soon
//! as the store's tip moves.

use std::sync::Arc;

use tokio::sync::Semaphore;
use zaino_chain_head::{HeadCapabilities, HeadCapability, StoreTip};
use zaino_primitives::types::{BlockRef, NetworkUpgradeInfo};

use crate::snapshot::PersistentSnapshot;

#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks",
    feature = "tree_state",
    feature = "history_tree"
))]
pub(crate) mod below;
pub(crate) mod folded;
pub(crate) mod inputs;

#[cfg(feature = "address_history")]
pub(crate) mod address_history;
#[cfg(feature = "compact_blocks")]
pub(crate) mod compact_blocks;
#[cfg(feature = "history_tree")]
pub(crate) mod history_tree;
#[cfg(feature = "transactions")]
pub(crate) mod transactions;
#[cfg(feature = "tree_state")]
pub(crate) mod tree_state;
#[cfg(feature = "txout_set")]
pub(crate) mod txout_set;

/// Blocks the root may lag the store watermark before the views are rebased.
pub(crate) const REBASE_EVERY: u32 = 32;

/// Why an extension stopped: its slot carries the reason until it is rebuilt.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub(crate) struct Failure(pub(crate) String);

impl Failure {
    pub(crate) fn of(what: &str, error: impl std::fmt::Display) -> Self {
        Self(format!("{what}: {error}"))
    }
}

/// One extension's place on a snapshot.
#[derive(Debug, Clone)]
pub(crate) enum Slot<T> {
    /// Built through the tip.
    Built(T),
    /// Failed; answers nothing until rebuilt.
    Failed(Failure),
}

impl<T> Slot<T> {
    pub(crate) fn built(&self) -> Option<&T> {
        match self {
            Self::Built(built) => Some(built),
            Self::Failed(_) => None,
        }
    }

    fn failure(&self) -> Option<&Failure> {
        match self {
            Self::Built(_) => None,
            Self::Failed(failure) => Some(failure),
        }
    }
}

/// The extension built in `slot`: `NotBuilt` when absent, `Failed` when it
/// failed.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks",
    feature = "tree_state",
    feature = "history_tree"
))]
pub(crate) fn built<'a, T>(
    slot: &'a Option<Slot<T>>,
    capability: &'static str,
) -> Result<&'a T, zaino_chain_head::ChainHeadError> {
    match slot {
        Some(Slot::Built(built)) => Ok(built),
        Some(Slot::Failed(failure)) => Err(zaino_chain_head::ChainHeadError::Failed {
            capability,
            reason: failure.0.clone(),
        }),
        None => Err(zaino_chain_head::ChainHeadError::NotBuilt { capability }),
    }
}

/// A built extension's failed read, as the head reports it.
pub(crate) fn read_failed(
    capability: &'static str,
) -> impl FnOnce(Failure) -> zaino_chain_head::ChainHeadError {
    move |failure| zaino_chain_head::ChainHeadError::ReadFailed {
        capability,
        reason: failure.0,
    }
}

/// The extensions' slots on a snapshot.
#[derive(Debug, Clone, Default)]
pub(crate) struct Slots {
    #[cfg(feature = "transactions")]
    pub(crate) transactions: Option<Slot<transactions::Transactions>>,
    #[cfg(feature = "txout_set")]
    pub(crate) txout_set: Option<Slot<txout_set::TxOutSet>>,
    #[cfg(feature = "address_history")]
    pub(crate) address_history: Option<Slot<address_history::AddressHistory>>,
    #[cfg(feature = "compact_blocks")]
    pub(crate) compact_blocks: Option<Slot<compact_blocks::CompactBlocks>>,
    #[cfg(feature = "tree_state")]
    pub(crate) tree_state: Option<Slot<tree_state::TreeState>>,
    #[cfg(feature = "history_tree")]
    pub(crate) history_tree: Option<Slot<history_tree::HistoryTree>>,
    /// The store's tip these slots were advanced with.
    store_tip: Option<BlockRef>,
}

impl Slots {
    /// Views of blocks the graph no longer retains dropped.
    pub(crate) fn retain(&mut self, graph: &PersistentSnapshot) {
        #[cfg(feature = "transactions")]
        if let Some(Slot::Built(built)) = &mut self.transactions {
            built.retain(graph);
        }
        #[cfg(feature = "txout_set")]
        if let Some(Slot::Built(built)) = &mut self.txout_set {
            built.retain(graph);
        }
        #[cfg(feature = "address_history")]
        if let Some(Slot::Built(built)) = &mut self.address_history {
            built.retain(graph);
        }
        #[cfg(feature = "compact_blocks")]
        if let Some(Slot::Built(built)) = &mut self.compact_blocks {
            built.retain(graph);
        }
        #[cfg(feature = "tree_state")]
        if let Some(Slot::Built(built)) = &mut self.tree_state {
            built.retain(graph);
        }
        #[cfg(feature = "history_tree")]
        if let Some(Slot::Built(built)) = &mut self.history_tree {
            built.retain(graph);
        }
        let _ = graph;
    }

    /// The failed slots, each with its reason.
    pub(crate) fn failures(&self) -> Vec<(HeadCapability, String)> {
        let mut failures = Vec::new();
        #[cfg(feature = "transactions")]
        if let Some(failure) = self.transactions.as_ref().and_then(Slot::failure) {
            failures.push((HeadCapability::Transactions, failure.0.clone()));
        }
        #[cfg(feature = "txout_set")]
        if let Some(failure) = self.txout_set.as_ref().and_then(Slot::failure) {
            failures.push((HeadCapability::TxOutSet, failure.0.clone()));
        }
        #[cfg(feature = "address_history")]
        if let Some(failure) = self.address_history.as_ref().and_then(Slot::failure) {
            failures.push((HeadCapability::TransparentHistory, failure.0.clone()));
        }
        #[cfg(feature = "compact_blocks")]
        if let Some(failure) = self.compact_blocks.as_ref().and_then(Slot::failure) {
            failures.push((HeadCapability::CompactBlocks, failure.0.clone()));
        }
        #[cfg(feature = "tree_state")]
        if let Some(failure) = self.tree_state.as_ref().and_then(Slot::failure) {
            failures.push((HeadCapability::TreeState, failure.0.clone()));
        }
        #[cfg(feature = "history_tree")]
        if let Some(failure) = self.history_tree.as_ref().and_then(Slot::failure) {
            failures.push((HeadCapability::HistoryTree, failure.0.clone()));
        }
        failures
    }

    /// What the built slots answer.
    pub(crate) fn capabilities(&self) -> HeadCapabilities {
        let mut built = HeadCapabilities::NONE;
        #[cfg(feature = "transactions")]
        if self
            .transactions
            .as_ref()
            .is_some_and(|slot| slot.built().is_some())
        {
            built = built.with(HeadCapability::Transactions);
        }
        #[cfg(feature = "txout_set")]
        if self
            .txout_set
            .as_ref()
            .is_some_and(|slot| slot.built().is_some())
        {
            built = built.with(HeadCapability::TxOutSet);
        }
        #[cfg(feature = "address_history")]
        if self
            .address_history
            .as_ref()
            .is_some_and(|slot| slot.built().is_some())
        {
            built = built.with(HeadCapability::TransparentHistory);
        }
        #[cfg(feature = "compact_blocks")]
        if self
            .compact_blocks
            .as_ref()
            .is_some_and(|slot| slot.built().is_some())
        {
            built = built.with(HeadCapability::CompactBlocks);
        }
        #[cfg(feature = "tree_state")]
        if self
            .tree_state
            .as_ref()
            .is_some_and(|slot| slot.built().is_some())
        {
            built = built.with(HeadCapability::TreeState);
        }
        #[cfg(feature = "history_tree")]
        if self
            .history_tree
            .as_ref()
            .is_some_and(|slot| slot.built().is_some())
        {
            built = built.with(HeadCapability::HistoryTree);
        }
        let _ = HeadCapability::ALL;
        built
    }
}

/// What the builder enabled, with the ports each extension drives.
pub(crate) struct Enabled {
    /// The validator's upgrade schedule: a block's consensus branch.
    pub(crate) schedule: Arc<[NetworkUpgradeInfo]>,
    /// The store's tip: the root the views sit on. `None`: no store.
    pub(crate) store_tip: Option<Arc<dyn StoreTip>>,
    /// Folds in flight at once, under the head's CPU share.
    pub(crate) folds: Arc<Semaphore>,
    /// Whether the wait for the store has been announced.
    #[cfg(any(
        feature = "txout_set",
        feature = "address_history",
        feature = "compact_blocks",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    pub(crate) store_wait: std::sync::atomic::AtomicBool,
    #[cfg(feature = "transactions")]
    pub(crate) transactions: bool,
    #[cfg(feature = "txout_set")]
    pub(crate) txout_set: Option<txout_set::Ports>,
    #[cfg(feature = "address_history")]
    pub(crate) address_history: Option<Arc<dyn below::Prevouts>>,
    #[cfg(feature = "compact_blocks")]
    pub(crate) compact_blocks: Option<Arc<dyn below::Prevouts>>,
    #[cfg(feature = "tree_state")]
    pub(crate) tree_state: Option<tree_state::Ports>,
    #[cfg(feature = "history_tree")]
    pub(crate) history_tree: Option<Arc<dyn below::HistorySeed>>,
}

impl std::fmt::Debug for Enabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Enabled").finish_non_exhaustive()
    }
}

impl Enabled {
    /// The store's tip now; `None` with no store or an empty one.
    fn store_tip(&self) -> Option<BlockRef> {
        self.store_tip.as_ref().and_then(|store| store.store_tip())
    }
}

/// Whether the store's tip moved since `slots` were advanced, so they should
/// be advanced again though the head's tip did not move.
pub(crate) fn store_moved(enabled: &Enabled, slots: &Slots) -> bool {
    enabled.store_tip() != slots.store_tip
}

/// The root the views sit on: the store's tip where it is a trunk block or
/// the floor's parent (the store built to the floor before the floor moved),
/// else the trunk's floor (a head with no store, or a store still behind).
///
/// `Err` names a store tip the trunk does not hold under that hash: the store
/// is on another chain, which the store's own tip check resolves.
fn root_of(graph: &PersistentSnapshot, watermark: Option<BlockRef>) -> Result<BlockRef, Failure> {
    let floor = graph.lowest_retained_height();
    let floor_block = graph
        .trunk_block(floor)
        .ok_or_else(|| Failure("the window has no floor".to_owned()))?;
    match watermark {
        Some(watermark) if watermark.hash == floor_block.parent_hash => Ok(watermark),
        Some(watermark) if watermark.height >= floor => match graph.trunk_block(watermark.height) {
            Some(block) if block.hash() == watermark.hash => Ok(block.reference),
            Some(block) => Err(Failure(format!(
                "the store watermark {} at {} is not the trunk's block {}",
                watermark.hash,
                watermark.height,
                block.hash()
            ))),
            None => Err(Failure(format!(
                "the store watermark {} is above the window's tip",
                watermark.height
            ))),
        },
        _ => Ok(floor_block.reference),
    }
}

/// Why nothing can be seeded yet: a store that has not built to the window's
/// floor holds no state at any root the window could sit on. Expected while
/// the store catches up, so it is a wait, not a failure.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks",
    feature = "tree_state",
    feature = "history_tree"
))]
fn store_behind(
    enabled: &Enabled,
    graph: &PersistentSnapshot,
    watermark: Option<BlockRef>,
) -> Option<Failure> {
    enabled.store_tip.as_ref()?;
    let needed = u32::from(graph.lowest_retained_height()).saturating_sub(1);
    let held = watermark.map(|tip| u32::from(tip.height));
    if held.is_some_and(|held| held >= needed) {
        return None;
    }
    Some(Failure(format!(
        "waiting for the store to reach height {needed}: it holds {}",
        held.map_or_else(|| "nothing".to_owned(), |held| held.to_string())
    )))
}

/// The wait as a slot, where `previous` has nothing built to keep: a built
/// extension goes on from its own views and is not darkened.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks",
    feature = "tree_state",
    feature = "history_tree"
))]
fn held<T>(waiting: Option<&Failure>, previous: Option<&Slot<T>>) -> Option<Slot<T>> {
    match (waiting, previous) {
        (None, _) | (Some(_), Some(Slot::Built(_))) => None,
        (Some(waiting), _) => Some(Slot::Failed(waiting.clone())),
    }
}

/// Says once that extensions began waiting for the store, and once that
/// none is any longer.
#[cfg(any(
    feature = "txout_set",
    feature = "address_history",
    feature = "compact_blocks",
    feature = "tree_state",
    feature = "history_tree"
))]
fn announce_wait(enabled: &Enabled, waiting: Option<&Failure>, next: &Slots) {
    use std::sync::atomic::Ordering;

    let held: Vec<HeadCapability> = waiting.map_or_else(Vec::new, |waiting| {
        next.failures()
            .into_iter()
            .filter(|(_, reason)| *reason == waiting.0)
            .map(|(capability, _)| capability)
            .collect()
    });
    match (held.is_empty(), waiting) {
        (false, Some(waiting)) => {
            if !enabled.store_wait.swap(true, Ordering::Relaxed) {
                tracing::info!(
                    extensions = ?held,
                    reason = %waiting,
                    "the chain head's extensions wait for the store to catch up: the validator answers their reads until then"
                );
            }
        }
        _ => {
            if enabled.store_wait.swap(false, Ordering::Relaxed) {
                tracing::info!(
                    "the store reached the chain head: building the chain head's extensions"
                );
            }
        }
    }
}

/// Whether a slot rooted at `current` should move its root to `watermark`.
fn due_for_rebase(current: BlockRef, watermark: BlockRef) -> bool {
    u32::from(watermark.height).saturating_sub(u32::from(current.height)) >= REBASE_EVERY
}

/// The slots after `graph` was built: each enabled extension seeded at the
/// root where it was not built, caught up to the graph, and rebased once the
/// root moved far enough. A failure darkens that slot alone.
pub(crate) async fn advance(
    enabled: &Enabled,
    previous: &Slots,
    graph: &PersistentSnapshot,
) -> Slots {
    let watermark = enabled.store_tip();
    let root = root_of(graph, watermark);
    let mut next = Slots {
        store_tip: watermark,
        ..Slots::default()
    };
    #[cfg(any(
        feature = "txout_set",
        feature = "address_history",
        feature = "compact_blocks",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    let waiting = store_behind(enabled, graph, watermark);

    #[cfg(feature = "transactions")]
    if enabled.transactions {
        next.transactions = Some(
            transactions::advance(enabled, previous.transactions.as_ref(), graph, &root).await,
        );
    }
    #[cfg(feature = "txout_set")]
    if let Some(ports) = &enabled.txout_set {
        next.txout_set = Some(match held(waiting.as_ref(), previous.txout_set.as_ref()) {
            Some(waiting) => waiting,
            None => {
                txout_set::advance(
                    enabled,
                    ports,
                    previous.txout_set.as_ref(),
                    graph,
                    &root,
                    next.transactions.as_ref().and_then(Slot::built),
                )
                .await
            }
        });
    }
    #[cfg(feature = "address_history")]
    if let Some(prevouts) = &enabled.address_history {
        next.address_history = Some(
            match held(waiting.as_ref(), previous.address_history.as_ref()) {
                Some(waiting) => waiting,
                None => {
                    address_history::advance(
                        enabled,
                        prevouts.as_ref(),
                        previous.address_history.as_ref(),
                        graph,
                        &root,
                        next.transactions.as_ref().and_then(Slot::built),
                    )
                    .await
                }
            },
        );
    }
    #[cfg(feature = "compact_blocks")]
    if let Some(prevouts) = &enabled.compact_blocks {
        next.compact_blocks = Some(
            match held(waiting.as_ref(), previous.compact_blocks.as_ref()) {
                Some(waiting) => waiting,
                None => {
                    compact_blocks::advance(
                        enabled,
                        prevouts.as_ref(),
                        previous.compact_blocks.as_ref(),
                        graph,
                        &root,
                        next.transactions.as_ref().and_then(Slot::built),
                    )
                    .await
                }
            },
        );
    }
    #[cfg(feature = "tree_state")]
    if let Some(ports) = &enabled.tree_state {
        next.tree_state = Some(match held(waiting.as_ref(), previous.tree_state.as_ref()) {
            Some(waiting) => waiting,
            None => {
                tree_state::advance(enabled, ports, previous.tree_state.as_ref(), graph, &root)
                    .await
            }
        });
    }
    #[cfg(feature = "history_tree")]
    if let Some(seed) = &enabled.history_tree {
        next.history_tree = Some(
            match held(waiting.as_ref(), previous.history_tree.as_ref()) {
                Some(waiting) => waiting,
                None => {
                    history_tree::advance(
                        enabled,
                        seed.as_ref(),
                        previous.history_tree.as_ref(),
                        graph,
                        &root,
                    )
                    .await
                }
            },
        );
    }
    #[cfg(any(
        feature = "txout_set",
        feature = "address_history",
        feature = "compact_blocks",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    announce_wait(enabled, waiting.as_ref(), &next);
    next
}

/// Logs and counts a failed extension; the reason goes into its slot.
pub(crate) fn failed<T>(extension: &'static str, failure: Failure) -> Slot<T> {
    tracing::error!(extension, %failure, "chain head extension failed: answering nothing until rebuilt");
    metrics::counter!(
        crate::metric_names::CHAIN_HEAD_EXTENSION_FAILURES_TOTAL,
        crate::metric_names::EXTENSION_LABEL => extension
    )
    .increment(1);
    Slot::Failed(failure)
}
