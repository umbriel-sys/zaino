//! Zaino's implementation of the ChainView ports.
//!
//! # The mechanism
//!
//! `Coverage` says who answers which heights; every read asks it and then
//! asks the provider it named. A point read is one segment, a range read is
//! several stitched in height order, and there is nothing between the reads and
//! that decision.
//!
//! # Generic over ports, not implementations
//!
//! The parameters are bounded by [`ChainStoreService`],
//! [`ChainHeadBlockService`] and [`ChainViewSource`] — ports, all three. No
//! adapter is named here, and each read trait is implemented under *its own*
//! bounds, so a store that builds only a compact index yields a view offering
//! compact reads and genuinely not offering spend status. Mixing this crate's
//! composer with someone else's store, or someone else's composer with these
//! ports, is the same amount of work.

pub(crate) mod config;
pub(crate) mod coverage;
pub(crate) mod fetch;
pub(crate) mod stream;
pub(crate) mod sync;

use std::sync::Arc;

use futures::Stream;
use std::collections::HashMap;
use tokio::sync::{watch, Semaphore};
use zaino_chain_head::{
    ChainHeadBlock, ChainHeadBlockService, ChainHeadError, ChainHeadHistoryTreeService,
    ChainHeadSnapshot, ChainHeadTransactionService, ChainHeadTransparentHistoryService,
    ChainHeadTreeStateService, ChainHeadTxOutSetService, HeadCapabilities, HeadCapability,
};
use zaino_chain_store::{
    ChainStoreError, ChainStoreReader, ChainStoreService,
    CompactBlockRead as StoreCompactBlockRead, HistoryEpoch, HistoryTreeIndex, PoolFilter,
    SpentOutputIndex, StoreCapability, TransactionIndex, TransparentHistoryIndex, TreeRootsIndex,
    TreeStateIndex, TxOutSetAccumulator, TxOutSetIndex,
};
use zaino_primitives::types::{
    legacy_tree_from_frontier, rpc::ChainTip, AbsoluteChainWork, AddressBalance, AddressDelta,
    BlockConfirmations, BlockHash, BlockHeader, BlockRef, ChainStateEpoch, CompactBlock, Height,
    Outpoint, PoolFrontier, PoolTreestate, ShieldedPool, SignedZatoshis, SubtreeIndex, SubtreeRoot,
    TransactionId, TransparentAddress, TransparentAddressKey, Treestate, Utxo, Zatoshis,
    ZatoshisFlowSum,
};

use crate::block::ChainBlock;
use crate::capability::{
    Answerable, ChainCapability, ServedCapabilities, ServiceabilityManifest, ServiceableRange,
};
use crate::error::{ChainViewError, Result};
use crate::ports::{
    AddressRead, BlockRead, ChainView, ChainViewSnapshot, CompactBlockRead, ForkReconcile,
    HistoryTreeRead, SpendRead, TransactionRead, TreestateRead, TxOutSetRead,
};
use crate::source::ChainViewSource;
use crate::types::{
    BlockId, ChainScope, ChainTxPosition, Locator, RawTransaction, SpendStatus,
    TransactionLocations,
};

use coverage::{Coverage, Provider};

pub use config::ChainViewConfig;
pub use sync::ChainViewSync;

/// A chain view over a store, a chain head, and a validator.
#[derive(Debug, Clone)]
pub struct ChainViewComposer<Store, Head, Source> {
    store: Store,
    head: Head,
    source: Arc<Source>,
    config: Arc<ChainViewConfig>,
    /// What this deployment has chosen to offer.
    ///
    /// Read by the manifest and by the reads alike, which is what stops
    /// "advertise" and "answer" drifting apart.
    served: ServedCapabilities,
    /// Bounds validator fetches across every client sharing this view.
    ///
    /// On the composer, not the snapshot: one per snapshot would be one per
    /// client, which is exactly the unbounded case this prevents.
    permits: Arc<Semaphore>,
    /// Runs between the head and store reads of `snapshot`.
    #[cfg(test)]
    between_reads: BetweenReads,
}

/// A test's hook between the head and store reads of a snapshot, where it
/// moves the store.
#[cfg(test)]
#[derive(Clone, Default)]
struct BetweenReads(Option<Arc<dyn Fn() + Send + Sync>>);

#[cfg(test)]
impl core::fmt::Debug for BetweenReads {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BetweenReads")
    }
}

impl<Store, Head, Source> ChainViewComposer<Store, Head, Source>
where
    Store: ChainStoreService,
    Head: ChainHeadBlockService,
    Source: ChainViewSource,
{
    /// A chain view over these three providers, offering everything they can
    /// answer.
    ///
    /// The right constructor for a deployment with nothing to withhold, which
    /// is every deployment in this workspace. Offering everything is not a
    /// claim that everything is answerable: the manifest still reports a
    /// capability the tiers cannot supply as
    /// [`Absent`](crate::Answerable::Absent), because it consults coverage and
    /// the store's index set as well.
    ///
    /// Use [`builder`](Self::builder) to offer less than the tiers can do.
    pub fn new(store: Store, head: Head, source: Arc<Source>, config: ChainViewConfig) -> Self {
        Self::assembled(store, head, source, config, ServedCapabilities::ALL)
    }

    /// A chain view offering only the capabilities named on the way out.
    ///
    /// Starts at [`ServedCapabilities::CORE`] — the capabilities every chain
    /// view answers — and takes the three optional ones by name. See
    /// [`ChainViewComposerBuilder`].
    pub fn builder(
        store: Store,
        head: Head,
        source: Arc<Source>,
    ) -> ChainViewComposerBuilder<Store, Head, Source> {
        ChainViewComposerBuilder {
            store,
            head,
            source,
            config: ChainViewConfig::default(),
            served: ServedCapabilities::CORE,
        }
    }

    /// The one assembly path, shared by both constructors.
    fn assembled(
        store: Store,
        head: Head,
        source: Arc<Source>,
        config: ChainViewConfig,
        served: ServedCapabilities,
    ) -> Self {
        let permits = Arc::new(Semaphore::new(config.passthrough_permits));
        Self {
            store,
            head,
            source,
            config: Arc::new(config),
            served,
            permits,
            #[cfg(test)]
            between_reads: BetweenReads::default(),
        }
    }

    /// This view with `hook` run between the head and store reads.
    #[cfg(test)]
    fn between_reads(mut self, hook: impl Fn() + Send + Sync + 'static) -> Self {
        self.between_reads = BetweenReads(Some(Arc::new(hook)));
        self
    }

    /// Captures the view every read is answered from.
    ///
    /// # Captured together, and only once
    ///
    /// The watermark and the chain-head snapshot are read here in one call.
    /// That is the coherence mechanism: read apart, the watermark could claim
    /// the store covers a height the pinned head still thinks is recent, and a
    /// read there would be answered by a store holding it under a *different
    /// hash* than the caller's view believes.
    ///
    /// Coverage is derived here too, so provider selection is later just
    /// arithmetic on two `Option<Height>`s. Reading the chain head's floor
    /// walks a boxed iterator, so doing it per read would allocate once per
    /// decision per client.
    pub fn snapshot(&self) -> ComposerSnapshot<Store::Reader, Head::Snapshot, Source> {
        // The head first: the store only advances, so the reader's tip is then
        // at or above the head's starting block and no range falls between.
        let head = self.head.current();
        #[cfg(test)]
        if let Some(hook) = &self.between_reads.0 {
            hook();
        }
        let reader = self.store.reader();

        let store_top = self
            .config
            .store_enabled
            .then(|| reader.watermark().tip.map(|tip| tip.height))
            .flatten();
        let head_range = head
            .best_chain()
            .next()
            .map(|floor| (floor.height(), head.best_tip().height));
        let work_anchor = head.work_anchor();

        ComposerSnapshot {
            reader,
            head,
            coverage: Coverage {
                store_top,
                head: head_range,
                work_anchor,
                source_fills: self.config.passthrough_enabled,
            },
            served: self.served,
            chainwork_offset: Arc::new(tokio::sync::OnceCell::new()),
            fetch: fetch::Fetcher::new(
                Arc::clone(&self.source),
                Arc::clone(&self.permits),
                Arc::clone(&self.config),
            ),
            config: Arc::clone(&self.config),
        }
    }

    /// What this view offers, and how far.
    ///
    /// Derived by walking [`ChainCapability::ALL`] against live coverage, so a
    /// capability cannot be advertised here and refused by a read.
    fn manifest(
        &self,
        store_capabilities: zaino_chain_store::StoreCapabilities,
    ) -> ServiceabilityManifest {
        let snapshot = self.snapshot();
        let coverage = snapshot.coverage;
        let head_capabilities = snapshot.head.capabilities();
        let config = &self.config;
        let served = self.served;

        ServiceabilityManifest::derive(|capability| {
            // Withheld by this deployment. Checked before anything about the
            // providers, because a capability nobody offers has no height to
            // report and the reads refuse it for the same reason.
            if !served.contains(capability) {
                return Answerable::Absent;
            }
            serviceability(
                capability,
                coverage,
                store_capabilities,
                head_capabilities,
                config,
            )
        })
    }
}

/// How far one capability is answerable.
///
/// The policy table, kept readable *as a table*: this is what a reviewer checks
/// when asking why a deployment does not advertise something.
fn serviceability(
    capability: ChainCapability,
    coverage: Coverage,
    store_capabilities: zaino_chain_store::StoreCapabilities,
    head_capabilities: HeadCapabilities,
    config: &ChainViewConfig,
) -> Answerable {
    let Some(tip) = coverage.chain_tip() else {
        return Answerable::NotAnswerable;
    };
    let has = |capability: StoreCapability| {
        config.store_enabled && store_capabilities.contains(capability)
    };
    let local = |store: StoreCapability, head: HeadCapability| {
        both_build(config, store_capabilities, store, head_capabilities, head)
    };
    let passthrough = config.passthrough_enabled;
    // Answered locally across the whole chain when nothing lies between the
    // halves, else not yet; the validator answers where `passthrough`.
    let merged = |store: StoreCapability, head: HeadCapability| match (
        passthrough,
        local(store, head),
        coverage.contiguous(),
    ) {
        (true, _, _) => Answerable::ToHeight(tip),
        (false, true, true) => Answerable::ToHeight(tip),
        (false, true, false) => Answerable::NotAnswerable,
        (false, false, _) => Answerable::Absent,
    };

    // The contiguous ceiling: everything up to here is answerable from the
    // local providers alone. Below a hole that is the store's top; with no hole
    // it is the tip.
    let local_ceiling = match coverage.gap_from() {
        None => Some(tip),
        Some(gap) => gap.checked_sub(1),
    };

    match capability {
        // Answered from whichever provider covers the height, and the validator
        // can stand in for either — so a hole does not cap these. An indexed
        // block joins the compact transactions with the tree roots, so the
        // store answers it only with both indexes.
        ChainCapability::Blocks | ChainCapability::CompactBlocks => {
            let local = has(StoreCapability::CompactBlocks)
                && (capability == ChainCapability::CompactBlocks
                    || has(StoreCapability::TreeRoots));
            if !local && !passthrough {
                return Answerable::Absent;
            }
            if passthrough {
                Answerable::ToHeight(tip)
            } else {
                local_ceiling.map_or(Answerable::NotAnswerable, Answerable::ToHeight)
            }
        }

        // No provider but the validator holds consensus bytes.
        ChainCapability::Transactions => {
            if passthrough {
                Answerable::ToHeight(tip)
            } else {
                Answerable::Absent
            }
        }

        // The store's trees to its tip, the head's above it; the validator
        // where either half is missing.
        ChainCapability::Treestate | ChainCapability::SubtreeRoots => {
            merged(StoreCapability::TreeState, HeadCapability::TreeState)
        }

        // The recent window's own graph; independent of what lies below it.
        ChainCapability::ChainTips => Answerable::ToHeight(tip),

        // Zaino's own indexes. The validator runs none of these, so a hole caps
        // them at the contiguous ceiling rather than being filled — and without
        // the index they are absent outright.
        ChainCapability::SpendStatus => {
            if !local(StoreCapability::SpentOutputs, HeadCapability::Transactions) {
                return Answerable::Absent;
            }
            local_ceiling.map_or(Answerable::NotAnswerable, Answerable::ToHeight)
        }
        // The head's `txout_set` extension: the store's set at its tip folded
        // up to the head's tip. Not answerable until the head has built it.
        ChainCapability::TxOutSet => {
            if !has(StoreCapability::TxOutSet) {
                return Answerable::Absent;
            }
            match head_capabilities.contains(HeadCapability::TxOutSet) {
                true => Answerable::ToHeight(tip),
                false => Answerable::NotAnswerable,
            }
        }
        // The store's effects to its tip, the head's above it; the validator
        // where either half is missing.
        ChainCapability::AddressHistory => merged(
            StoreCapability::TransparentHistory,
            HeadCapability::TransparentHistory,
        ),
        // The store's tree to its tip, the head's above it. No validator
        // answers it.
        ChainCapability::HistoryTree => {
            if !local(StoreCapability::HistoryTree, HeadCapability::HistoryTree) {
                return Answerable::Absent;
            }
            match coverage.contiguous() {
                true => Answerable::ToHeight(tip),
                false => Answerable::NotAnswerable,
            }
        }
    }
}

impl<Store, Head, Source> ChainView for ChainViewComposer<Store, Head, Source>
where
    Store: ChainStoreService<
        Reader: TreeRootsIndex + StoreCompactBlockRead + TransactionIndex + TreeStateIndex,
    >,
    Head: ChainHeadBlockService<Snapshot: ChainHeadTransactionService + ChainHeadTreeStateService>,
    Source: ChainViewSource,
{
    type Snapshot = ComposerSnapshot<Store::Reader, Head::Snapshot, Source>;

    fn snapshot(&self) -> Self::Snapshot {
        ChainViewComposer::snapshot(self)
    }

    fn subscribe_tip(&self) -> watch::Receiver<ChainStateEpoch> {
        self.head.subscribe_updates()
    }

    fn serviceability(&self) -> ServiceabilityManifest {
        self.manifest(self.store.reader().capabilities())
    }
}

/// Assembles a chain view that offers less than its tiers could.
///
/// Starts at [`ServedCapabilities::CORE`] and takes the three optional
/// capabilities by name.
///
/// # The methods are type-gated
///
/// Each `serving_*` method exists **only** when both tiers can supply its half.
/// `serving_spend_status` requires the store's reader to implement
/// [`SpentOutputIndex`] and the head's snapshot to implement
/// [`ChainHeadTransactionService`]; a composition missing either does not have
/// the method, so the advertisement cannot be made.
///
/// That is the property worth having. A runtime check would let a deployment
/// advertise a capability, fail every read of it, and look like a fault rather
/// than a misconfiguration — and it would be discovered by a client, in
/// production, rather than by the compiler.
///
/// It is a floor, not a ceiling. Naming a capability offers it; whether it is
/// *answerable*, and to what height, is still decided by the manifest from live
/// coverage and the store's runtime index set. A deployment can offer spend
/// status and have the manifest report `NotAnswerable` while the store is still
/// building.
pub struct ChainViewComposerBuilder<Store, Head, Source> {
    store: Store,
    head: Head,
    source: Arc<Source>,
    config: ChainViewConfig,
    served: ServedCapabilities,
}

impl<Store, Head, Source> ChainViewComposerBuilder<Store, Head, Source>
where
    Store: ChainStoreService,
    Head: ChainHeadBlockService,
    Source: ChainViewSource,
{
    /// Uses this configuration rather than the default.
    pub fn with_config(mut self, config: ChainViewConfig) -> Self {
        self.config = config;
        self
    }

    /// Offers spend status: whether an outpoint is spent, and by what.
    ///
    /// A merge, so both tiers must supply their half — the store for spends
    /// below the seam, the head for spends inside the window.
    pub fn serving_spend_status(mut self) -> Self
    where
        Store::Reader: SpentOutputIndex,
        Head::Snapshot: ChainHeadTransactionService,
    {
        self.served = self.served.with(ChainCapability::SpendStatus);
        self
    }

    /// Offers the unspent transparent output set's running totals at the tip.
    ///
    /// A merge like spend status: the store's accumulator extended by what the
    /// window created and spent, resolved through the store's spend indexes.
    pub fn serving_txout_set(mut self) -> Self
    where
        Store::Reader: TxOutSetIndex + SpentOutputIndex + TransactionIndex,
        Head::Snapshot: ChainHeadTxOutSetService,
    {
        self.served = self.served.with(ChainCapability::TxOutSet);
        self
    }

    /// Offers transparent address history.
    ///
    /// A merge like spend status: the store's finalised effects joined with the
    /// window's recent ones.
    pub fn serving_address_history(mut self) -> Self
    where
        Store::Reader: zaino_chain_store::TransparentHistoryIndex,
        Head::Snapshot: zaino_chain_head::ChainHeadTransparentHistoryService,
    {
        self.served = self.served.with(ChainCapability::AddressHistory);
        self
    }

    /// Offers the chain history tree: the store's to its tip, the head's above.
    pub fn serving_history_tree(mut self) -> Self
    where
        Store::Reader: HistoryTreeIndex,
        Head::Snapshot: ChainHeadHistoryTreeService,
    {
        self.served = self.served.with(ChainCapability::HistoryTree);
        self
    }

    /// The chain view.
    pub fn build(self) -> ChainViewComposer<Store, Head, Source> {
        ChainViewComposer::assembled(self.store, self.head, self.source, self.config, self.served)
    }
}

/// A chain view pinned to one tip.
pub struct ComposerSnapshot<Reader, HeadSnapshot, Source> {
    reader: Reader,
    head: Arc<HeadSnapshot>,
    /// What each provider covers, captured with the tiers themselves.
    coverage: Coverage,
    /// What this deployment offers. The same set the manifest reports from, so
    /// a read cannot answer something the manifest called absent.
    served: ServedCapabilities,
    /// The anchor's absolute chainwork, resolved at most once.
    ///
    /// Shared across clones rather than copied: a clone is the same pinned
    /// view, so it is the same answer, and resolving it costs a store read.
    /// The inner `None` — the store has not built as far as the anchor — is
    /// cached like any other answer, because coverage is pinned too and so
    /// cannot become resolvable within one snapshot.
    chainwork_offset: Arc<tokio::sync::OnceCell<AnchorChainWork>>,
    fetch: fetch::Fetcher<Source>,
    config: Arc<ChainViewConfig>,
}

/// Cloning shares the providers rather than duplicating them.
///
/// Hand-written: the derive would demand `HeadSnapshot: Clone` and
/// `Source: Clone`, which is what the `Arc`s exist to avoid. Cheap enough that
/// a stream can own a clone, which is what makes a returned stream `'static`.
impl<Reader: Clone, HeadSnapshot, Source> Clone for ComposerSnapshot<Reader, HeadSnapshot, Source> {
    fn clone(&self) -> Self {
        Self {
            reader: self.reader.clone(),
            head: Arc::clone(&self.head),
            coverage: self.coverage,
            served: self.served,
            chainwork_offset: Arc::clone(&self.chainwork_offset),
            fetch: self.fetch.clone(),
            config: Arc::clone(&self.config),
        }
    }
}

impl<Reader, HeadSnapshot, Source> core::fmt::Debug
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ComposerSnapshot")
            .field("coverage", &self.coverage)
            .finish_non_exhaustive()
    }
}

/// A store failure, classified for a consumer.
pub(crate) fn store_err(error: ChainStoreError) -> ChainViewError {
    match error {
        ChainStoreError::NotReady => {
            ChainViewError::Transient(String::from("finalised store is not ready"))
        }
        ChainStoreError::Unavailable(index) => ChainViewError::NotServiceable(match index {
            zaino_chain_store::StoreCapability::CompactBlocks => {
                "the store does not build the compact-block index"
            }
            zaino_chain_store::StoreCapability::SpentOutputs => {
                "the store does not build the spent-output index"
            }
            zaino_chain_store::StoreCapability::TxOutSet => {
                "the store does not build the txout-set accumulator"
            }
            _ => "the store does not build a required index",
        }),
        // Coverage decides which provider answers *before* asking, so either of
        // these means it chose wrongly — a fault here, not a condition there.
        // Reporting them as `NotServiceable` would make a routing bug look like
        // a store that has not caught up, which happens often enough that
        // nobody would look twice.
        other @ (ChainStoreError::AboveWatermark { .. } | ChainStoreError::InvalidRange { .. }) => {
            ChainViewError::Fatal(format!("chain view routed a read wrongly: {other}"))
        }
        other => ChainViewError::Fatal(other.to_string()),
    }
}

/// A chain-head failure, classified for a consumer: a capability the head
/// does not build (not enabled, or failed and not yet rebuilt) is not
/// serviceable; anything else is a fault.
pub(crate) fn head_err(error: ChainHeadError) -> ChainViewError {
    match error {
        ChainHeadError::NotBuilt { .. } => {
            ChainViewError::NotServiceable("the chain head does not build this capability")
        }
        other => ChainViewError::Fatal(other.to_string()),
    }
}

/// A chain-head block's work made absolute, given the anchor's chainwork.
///
/// `None` when the anchor's chainwork is unknown, and also on overflow — which
/// no real chain reaches, and which is reported absent rather than wrong for
/// the same reason every other unknown is.
fn rebase(anchor: AnchorChainWork, block: &ChainHeadBlock) -> Option<AbsoluteChainWork> {
    let relative = u128::from(block.work);
    let absolute = match anchor {
        AnchorChainWork::Known(anchor) => core::num::NonZeroU128::from(anchor)
            .get()
            .checked_add(relative)?,
        AnchorChainWork::Unknown => return None,
    };
    core::num::NonZeroU128::new(absolute).map(AbsoluteChainWork::new)
}

/// The absolute chainwork of the chain head's anchor.
#[derive(Debug, Clone, Copy)]
enum AnchorChainWork {
    /// The anchor's absolute chainwork, read from the store.
    Known(AbsoluteChainWork),
    /// The store has not built up to the anchor yet.
    Unknown,
}

/// This deployment does not offer `capability`.
///
/// Distinct from the tiers being unable to answer: they may well be able, and
/// the deployment has chosen not to. The message says so, because "not
/// serviceable" reads as a fault otherwise and an operator needs to know which
/// of the two to go and change.
///
/// The manifest reports the same capabilities as
/// [`Absent`](crate::Answerable::Absent) from the same set, so a consumer that
/// checked first never reaches this.
fn withheld(capability: ChainCapability) -> ChainViewError {
    ChainViewError::NotServiceable(match capability {
        ChainCapability::Blocks => "this deployment does not serve blocks",
        ChainCapability::CompactBlocks => "this deployment does not serve compact blocks",
        ChainCapability::Transactions => "this deployment does not serve transactions",
        ChainCapability::Treestate => "this deployment does not serve treestates",
        ChainCapability::SubtreeRoots => "this deployment does not serve subtree roots",
        ChainCapability::ChainTips => "this deployment does not serve chain tips",
        ChainCapability::AddressHistory => {
            "this deployment does not serve transparent address history"
        }
        ChainCapability::SpendStatus => "this deployment does not serve spend status",
        ChainCapability::TxOutSet => "this deployment does not serve the txout set",
        ChainCapability::HistoryTree => "this deployment does not serve the history tree",
    })
}

/// The chain view cannot cover a height from any provider.
fn uncoverable() -> ChainViewError {
    ChainViewError::NotServiceable("no provider covers this height and the validator is disabled")
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    HeadSnapshot: ChainHeadSnapshot,
{
    /// The block the chain head holds at `height`, if it holds one.
    fn head_block(&self, height: Height) -> Option<&ChainHeadBlock> {
        self.head.best_block_by_height(height)
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader,
    HeadSnapshot: ChainHeadSnapshot,
{
    /// A chain-head block's work, made absolute.
    ///
    /// The chain head counts work from the block above its anchor, and the
    /// anchor's chainwork includes the anchor itself, so this is one addition:
    ///
    /// ```text
    /// absolute(B) = chainwork(anchor) + work(B)
    /// ```
    ///
    /// `None` when the anchor's chainwork is not known, which is the honest
    /// answer rather than a placeholder: see [`ChainBlock::chainwork`].
    ///
    /// Correct for a competing block as well as a canonical one. Every block
    /// the head retains accumulates from the same anchor, and a competing
    /// block's chainwork is a real quantity — it is what makes one branch
    /// heavier than another.
    async fn absolute_chainwork(
        &self,
        block: &ChainHeadBlock,
    ) -> Result<Option<AbsoluteChainWork>> {
        Ok(rebase(self.chainwork_offset().await?, block))
    }

    /// The absolute chainwork of the chain head's anchor.
    ///
    /// Read from the store, because the store is the only provider whose
    /// coverage reaches genesis and so the only one that can know a cumulative
    /// quantity. `None` while it has not built that far: there is no correct
    /// value to serve until then.
    ///
    /// Resolved at most once per snapshot and shared by every read and every
    /// clone.
    ///
    /// Looking the anchor up by height is sound: the store is append-only, so
    /// it never holds a different block at a height it has already built.
    /// `rewind_to` is a repair path with no caller outside the store.
    async fn chainwork_offset(&self) -> Result<AnchorChainWork> {
        self.chainwork_offset
            .get_or_try_init(|| async {
                let anchor = self.coverage.work_anchor;

                if self
                    .coverage
                    .store_top
                    .is_none_or(|top| top < anchor.height)
                {
                    return Ok(AnchorChainWork::Unknown);
                }

                Ok(self
                    .reader
                    .chainwork(anchor.height)
                    .await
                    .map_err(store_err)?
                    .map_or(AnchorChainWork::Unknown, AnchorChainWork::Known))
            })
            .await
            .copied()
    }
}

impl<Reader, HeadSnapshot, Source> stream::Coverable
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
{
    fn coverage(&self) -> Coverage {
        self.coverage
    }

    fn chunk_budget_bytes(&self) -> usize {
        self.config.chunk_budget_bytes
    }
}

// ***** Chain shape *****

impl<Reader, HeadSnapshot, Source> ChainViewSnapshot
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader
        + TreeRootsIndex
        + StoreCompactBlockRead
        + TransactionIndex
        + TreeStateIndex,
    HeadSnapshot: ChainHeadTransactionService + ChainHeadTreeStateService,
    Source: ChainViewSource,
{
    fn tip(&self) -> BlockRef {
        self.head.best_tip()
    }

    fn epoch(&self) -> ChainStateEpoch {
        self.head.epoch()
    }

    fn serviceable_range(&self) -> ServiceableRange {
        ServiceableRange {
            finalised_tip: self.coverage.store_top,
            tip: self.coverage.chain_tip(),
            gap_from: self.coverage.gap_from(),
        }
    }
}

impl<Reader, HeadSnapshot, Source> ForkReconcile for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TreeRootsIndex + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    fn chain_tips(&self) -> Vec<ChainTip> {
        self.head.chain_tips()
    }

    async fn fork_point(&self, locator: &Locator) -> Result<Option<BlockRef>> {
        for hash in locator.hashes() {
            if self.head.block_by_hash(hash).is_some() {
                match self.head.find_fork_point(hash) {
                    Some(fork) => return Ok(Some(fork)),
                    None => continue,
                }
            }
            if self.coverage.store_top.is_some() {
                if let Some(height) = self.reader.block_height(*hash).await.map_err(store_err)? {
                    return Ok(Some(BlockRef {
                        hash: *hash,
                        height,
                    }));
                }
            }
        }
        Ok(None)
    }

    fn stream_blocks_to_tip(
        &self,
        from: Height,
    ) -> impl Stream<Item = Result<Vec<ChainBlock>>> + Send + use<Reader, HeadSnapshot, Source>
    {
        let end = self
            .coverage
            .chain_tip()
            .unwrap_or(Height::GENESIS)
            .max(from);
        self.stream_blocks(from, end)
    }
}

// ***** Blocks *****

impl<Reader, HeadSnapshot, Source> BlockRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TreeRootsIndex + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    async fn block_hash(&self, height: Height) -> Result<Option<BlockHash>> {
        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(Provider::Store) => self.reader.block_hash(height).await.map_err(store_err),
            Some(Provider::Head) => Ok(self.head_block(height).map(ChainHeadBlock::hash)),
            Some(Provider::Source) => Ok(self
                .fetch
                .block_at(height)
                .await?
                .map(|(block, _)| block.header.hash)),
        }
    }

    async fn block_height(&self, hash: BlockHash) -> Result<Option<Height>> {
        // Id-addressed: there is no height to select a provider with, so ask
        // the providers that index by hash and then the validator.
        //
        // The chain head first — an in-memory lookup where the store's is a
        // disk read, and the only provider retaining competing branches, so the
        // only one that can tell a canonical block from an orphaned one.
        if let Some(block) = self.head.block_by_hash(&hash) {
            return Ok(self
                .head
                .is_on_best_chain(block.reference)
                .then(|| block.height()));
        }

        if self.coverage.store_top.is_some() {
            if let Some(height) = self.reader.block_height(hash).await.map_err(store_err)? {
                return Ok(Some(height));
            }
        }

        // Neither holds it. During catch-up that is the ordinary case for any
        // historical hash, so ask the validator rather than reporting a block
        // that plainly exists as absent.
        //
        // The validator answers against its own best chain, so its answer
        // stands only at a height no provider pins: a covered height holds a
        // different block in this view, and one above the tip is not in it.
        if !self.fetch.enabled() {
            return Ok(None);
        }
        let Some(header) = self.fetch.block_header(hash).await? else {
            return Ok(None);
        };
        let canonical = header.confirmations != BlockConfirmations::NotInBestChain;
        let unpinned = matches!(
            self.coverage.provider_at(header.height),
            Ok(Some(Provider::Source))
        );
        Ok((canonical && unpinned).then_some(header.height))
    }

    async fn block(&self, at: BlockId) -> Result<Option<ChainBlock>> {
        let height = match at {
            BlockId::Height(height) => height,
            BlockId::Hash(hash) => {
                // The chain head can answer for a competing block, which no
                // height names — so try it before resolving.
                if let Some(block) = self.head.block_by_hash(&hash) {
                    let chainwork = self.absolute_chainwork(block).await?;
                    return Ok(Some(ChainBlock::from_parsed(
                        &block.block,
                        block.tree_roots.clone(),
                        chainwork,
                    )));
                }
                match self.block_height(hash).await? {
                    Some(height) => height,
                    None => return Ok(None),
                }
            }
        };

        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(Provider::Store) => Ok(self
                .blocks_from(Provider::Store, vec![height])
                .await?
                .into_iter()
                .next()),
            Some(Provider::Head) => match self.head_block(height) {
                None => Ok(None),
                Some(block) => {
                    let chainwork = self.absolute_chainwork(block).await?;
                    Ok(Some(ChainBlock::from_parsed(
                        &block.block,
                        block.tree_roots.clone(),
                        chainwork,
                    )))
                }
            },
            // The validator reports no work through this path, so there is
            // nothing to rebase and nothing to serve.
            Some(Provider::Source) => Ok(self
                .fetch
                .block_at(height)
                .await?
                .map(|(block, roots)| ChainBlock::from_parsed(&block, roots, None))),
        }
    }

    async fn block_header(&self, at: BlockId) -> Result<Option<BlockHeader>> {
        // A store-covered height needs only the headers index.
        if let BlockId::Height(height) = at {
            if let Ok(Some(Provider::Store)) = self.coverage.provider_at(height) {
                return Ok(self
                    .reader
                    .headers_chunk(height, height)
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .next()
                    .map(|stored| stored.header));
            }
        }
        Ok(self.block(at).await?.map(|block| block.header))
    }

    async fn raw_block(&self, at: BlockId) -> Result<Option<Vec<u8>>> {
        match at {
            BlockId::Height(height) => match self
                .coverage
                .provider_at(height)
                .map_err(|()| uncoverable())?
            {
                None => Ok(None),
                Some(provider) => Ok(self
                    .raw_blocks_from(provider, vec![height])
                    .await?
                    .into_iter()
                    .next()),
            },
            BlockId::Hash(hash) => {
                self.fetch
                    .require("raw blocks need the validator, which is disabled")?;
                fetch::miss(self.fetch.source().get_raw_block_by_hash(hash).await)
            }
        }
    }

    fn stream_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Vec<ChainBlock>>> + Send + use<Reader, HeadSnapshot, Source>
    {
        stream::walk(
            self.clone(),
            start,
            end,
            move |snapshot, segment, heights| {
                Box::pin(async move { snapshot.blocks_from(segment.provider, heights).await })
            },
        )
    }

    fn stream_raw_blocks(
        &self,
        start: Height,
        end: Height,
    ) -> impl Stream<Item = Result<Vec<Vec<u8>>>> + Send + use<Reader, HeadSnapshot, Source> {
        stream::walk(
            self.clone(),
            start,
            end,
            move |snapshot, segment, heights| {
                Box::pin(async move { snapshot.raw_blocks_from(segment.provider, heights).await })
            },
        )
    }
}

// ***** Per-provider batch reads, shared by the point and range paths *****

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    /// The hash this snapshot holds at `height`, or `None` where no local
    /// provider covers it.
    async fn pinned_hash(&self, provider: Provider, height: Height) -> Result<Option<BlockHash>> {
        let hash = match provider {
            Provider::Source => return Ok(None),
            Provider::Store => self.reader.block_hash(height).await.map_err(store_err)?,
            Provider::Head => self.head_block(height).map(ChainHeadBlock::hash),
        };
        hash.map(Some).ok_or_else(|| unpinned(height))
    }

    /// Consensus bytes for a run of heights, pinned to this snapshot.
    ///
    /// No provider retains consensus bytes, so the validator serves them all.
    /// Where a local provider covers the run, each block is fetched by the hash
    /// this snapshot holds at that height, so a reorg since the snapshot was
    /// taken cannot substitute a block the snapshot never saw.
    async fn raw_blocks_from(
        &self,
        provider: Provider,
        heights: Vec<Height>,
    ) -> Result<Vec<Vec<u8>>> {
        self.fetch
            .require("raw blocks need the validator, which is disabled")?;
        if provider == Provider::Source {
            return self.fetch.fill_raw_blocks(heights).await;
        }
        let mut hashes = Vec::with_capacity(heights.len());
        for height in heights {
            hashes.push(
                self.pinned_hash(provider, height)
                    .await?
                    .ok_or_else(|| unpinned(height))?,
            );
        }

        let expected = hashes.len();
        let blocks = self.fetch.fill_raw_blocks_by_hash(hashes).await?;
        if blocks.len() != expected {
            return Err(ChainViewError::Transient(String::from(
                "the validator no longer serves a block this snapshot pins; retry with a fresh snapshot",
            )));
        }
        Ok(blocks)
    }
}

/// The store's indexes answered a run with different blocks or lengths.
fn disagreeing_indexes(first: Height, last: Height) -> ChainViewError {
    ChainViewError::Fatal(format!(
        "the store's headers, compact transactions and tree roots disagree over {first}..={last}"
    ))
}

/// Coverage named a local provider for a height it holds no block at.
fn unpinned(height: Height) -> ChainViewError {
    ChainViewError::Fatal(format!(
        "chain view routed height {height} to a provider holding no block there"
    ))
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TreeRootsIndex + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    /// Indexed blocks for a run of heights, from one provider.
    async fn blocks_from(
        &self,
        provider: Provider,
        heights: Vec<Height>,
    ) -> Result<Vec<ChainBlock>> {
        let (Some(first), Some(last)) = (heights.first().copied(), heights.last().copied()) else {
            return Ok(Vec::new());
        };
        match provider {
            // Three index reads joined by height: the headers with their work,
            // every transaction in compact form, the roots after each block.
            // Coverage routes only heights the store held when the snapshot
            // was taken, so none of the three is truncated, and a run that
            // disagrees with itself is a damaged store, not a race.
            Provider::Store => {
                let headers = self
                    .reader
                    .headers_chunk(first, last)
                    .await
                    .map_err(store_err)?;
                let compact = self
                    .reader
                    .compact_chunk(first, last, PoolFilter::all())
                    .await
                    .map_err(store_err)?;
                let roots = self
                    .reader
                    .tree_roots_chunk(first, last)
                    .await
                    .map_err(store_err)?;
                if headers.len() != compact.len() || headers.len() != roots.len() {
                    return Err(disagreeing_indexes(first, last));
                }
                headers
                    .into_iter()
                    .zip(compact)
                    .zip(roots)
                    .map(|((stored, compact), tree_roots)| {
                        if compact.hash != stored.header.hash
                            || compact.height != u32::from(stored.header.height)
                        {
                            return Err(disagreeing_indexes(first, last));
                        }
                        Ok(ChainBlock {
                            header: stored.header,
                            transactions: compact.transactions,
                            tree_roots,
                            chainwork: Some(stored.chainwork),
                            size: stored.size,
                        })
                    })
                    .collect()
            }
            Provider::Head => {
                // Resolved once for the run rather than per block: it is the
                // same anchor for every block in the window.
                let offset = self.chainwork_offset().await?;
                Ok(self
                    .head
                    .best_chain_blocks(first, last)
                    .map_err(|error| ChainViewError::Fatal(error.to_string()))?
                    .map(|block| {
                        ChainBlock::from_parsed(
                            &block.block,
                            block.tree_roots.clone(),
                            rebase(offset, block),
                        )
                    })
                    .collect())
            }
            // The validator reports no work through this path, so there is
            // nothing to rebase and nothing to serve.
            Provider::Source => Ok(self
                .fetch
                .fill_blocks(heights)
                .await?
                .into_iter()
                .map(|(block, roots)| ChainBlock::from_parsed(&block, roots, None))
                .collect()),
        }
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    /// Compact blocks for a run of heights, from one provider.
    async fn compact_from(
        &self,
        provider: Provider,
        heights: Vec<Height>,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Vec<CompactBlock>> {
        let (Some(first), Some(last)) = (heights.first().copied(), heights.last().copied()) else {
            return Ok(Vec::new());
        };
        match provider {
            // The filter is pushed into the store's read, where it decides
            // which row families decode at all.
            Provider::Store => self
                .reader
                .compact_chunk(first, last, pools)
                .await
                .map_err(store_err),
            Provider::Head => Ok(self
                .head
                .best_chain_blocks(first, last)
                .map_err(|error| ChainViewError::Fatal(error.to_string()))?
                .map(|block| {
                    stream::compact_from_pre_index(
                        zaino_primitives::types::PreIndexCompactBlock::from(&block.block),
                        stream::metadata_from_roots(&block.tree_roots),
                        pools,
                    )
                })
                .collect()),
            Provider::Source => self.fetch.fill_compact(heights, pools).await,
        }
    }
}

// ***** Compact blocks *****

impl<Reader, HeadSnapshot, Source> CompactBlockRead
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadSnapshot,
    Source: ChainViewSource,
{
    async fn compact_block(
        &self,
        height: Height,
        pools: zaino_chain_store::PoolFilter,
    ) -> Result<Option<CompactBlock>> {
        match self
            .coverage
            .provider_at(height)
            .map_err(|()| uncoverable())?
        {
            None => Ok(None),
            Some(provider) => Ok(self
                .compact_from(provider, vec![height], pools)
                .await?
                .into_iter()
                .next()),
        }
    }

    fn stream_compact(
        &self,
        start: Height,
        end: Height,
        pools: zaino_chain_store::PoolFilter,
    ) -> impl Stream<Item = Result<Vec<CompactBlock>>> + Send + use<Reader, HeadSnapshot, Source>
    {
        stream::walk_sized(
            self.clone(),
            start,
            end,
            move |snapshot, segment, heights| {
                Box::pin(async move {
                    snapshot
                        .compact_from(segment.provider, heights, pools)
                        .await
                })
            },
            |blocks| blocks.iter().map(stream::approx_size).sum(),
        )
    }
}

// ***** Transactions *****

impl<Reader, HeadSnapshot, Source> TransactionRead
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TransactionIndex,
    HeadSnapshot: ChainHeadSnapshot + ChainHeadTransactionService,
    Source: ChainViewSource,
{
    async fn raw_transaction(&self, txid: TransactionId) -> Result<Option<RawTransaction>> {
        self.fetch
            .require("raw transactions need the validator, which is disabled")?;
        Ok(
            fetch::miss(self.fetch.source().get_transaction(txid).await)?.map(|response| {
                RawTransaction {
                    bytes: response.bytes,
                    location: response.location,
                }
            }),
        )
    }

    async fn transaction_locations(&self, txid: TransactionId) -> Result<TransactionLocations> {
        // The chain head answers first and answers most: it is the only
        // provider retaining competing branches, so the only source of the
        // non-best half.
        let recent = self.head.transaction_locations(&txid).map_err(head_err)?;
        let mut locations = TransactionLocations {
            best_chain: recent.best_chain.map(|at| ChainTxPosition {
                block: at.block,
                tx_index: at.tx_index,
            }),
            non_best_chain: recent
                .non_best_chain
                .into_iter()
                .map(|at| ChainTxPosition {
                    block: at.block,
                    tx_index: at.tx_index,
                })
                .collect(),
        };

        if locations.best_chain.is_some() || self.coverage.store_top.is_none() {
            return Ok(locations);
        }

        // Below the window there is a single chain, so the store's
        // best-chain-only index is the whole answer for its range.
        let Some(position) = self.reader.tx_position(&txid).await.map_err(store_err)? else {
            return Ok(locations);
        };
        // The store answers with a height; a position names a block, so
        // complete it with the hash at that height, which it must hold.
        let hash = self
            .reader
            .block_hash(position.height)
            .await
            .map_err(store_err)?
            .ok_or_else(|| unpinned(position.height))?;
        locations.best_chain = Some(ChainTxPosition {
            block: BlockRef {
                hash,
                height: position.height,
            },
            tx_index: position.tx_index,
        });
        Ok(locations)
    }
}

// ***** Commitment trees *****

impl<Reader, HeadSnapshot, Source> TreestateRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TreeStateIndex + TreeRootsIndex + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadTreeStateService,
    Source: ChainViewSource,
{
    async fn treestate(&self, at: BlockId) -> Result<Option<Treestate>> {
        if let Some(local) = self.local_treestate(at).await? {
            return Ok(Some(local));
        }
        // The validator's answer: by the hash this snapshot pins where a
        // provider covers the height, by height only in a hole. The tree port
        // leaves each pool's root unset, so the roots are read by the
        // treestate's own hash and joined in.
        self.fetch
            .require("treestates need the validator, which is disabled")?;
        let source = self.fetch.source();
        let treestate = match at {
            BlockId::Hash(hash) => fetch::miss(source.get_treestate_by_hash(hash).await),
            BlockId::Height(height) => {
                let Some(provider) = self
                    .coverage
                    .provider_at(height)
                    .map_err(|()| uncoverable())?
                else {
                    return Ok(None);
                };
                match self.pinned_hash(provider, height).await? {
                    Some(hash) => fetch::miss(source.get_treestate_by_hash(hash).await),
                    None => fetch::miss(source.get_treestate(height).await),
                }
            }
        }?;
        let Some(mut treestate) = treestate else {
            return Ok(None);
        };
        let roots = fetch::miss(source.get_commitment_tree_roots(treestate.block_hash).await)?
            .ok_or_else(|| {
                ChainViewError::Transient(String::from(
                    "the validator no longer holds the block its treestate named",
                ))
            })?;
        treestate.sapling = with_root(treestate.sapling, roots.sapling);
        treestate.orchard = with_root(treestate.orchard, roots.orchard);
        treestate.ironwood = with_root(treestate.ironwood, roots.ironwood);
        Ok(Some(treestate))
    }

    async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        limit: Option<u16>,
    ) -> Result<Vec<SubtreeRoot>> {
        if let Some(local) = self.local_subtree_roots(pool, start_index, limit).await? {
            return Ok(local);
        }
        // The port's only domain error is an inactive pool, an invalid request.
        self.fetch
            .require("subtree roots need the validator, which is disabled")?;
        fetch::answered(
            self.fetch
                .source()
                .get_subtree_roots(pool, start_index, limit)
                .await,
        )
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TreeStateIndex + TreeRootsIndex + StoreCompactBlockRead,
    HeadSnapshot: ChainHeadTreeStateService,
    Source: ChainViewSource,
{
    /// Whether the store's trees to its tip and the head's above it cover the
    /// chain with nothing between them.
    fn local_trees(&self) -> bool {
        both_build(
            &self.config,
            self.reader.capabilities(),
            StoreCapability::TreeState,
            self.head.capabilities(),
            HeadCapability::TreeState,
        ) && self.coverage.contiguous()
    }

    /// The treestate from the store (at or below its tip) or the head (above
    /// it); `None` sends the read to the validator: the trees not built
    /// locally, or `at` names a block off this snapshot's best chain.
    async fn local_treestate(&self, at: BlockId) -> Result<Option<Treestate>> {
        if !self.local_trees() {
            return Ok(None);
        }
        let height = match at {
            BlockId::Height(height) => height,
            BlockId::Hash(hash) => match self.block_height(hash).await? {
                Some(height) => height,
                None => return Ok(None),
            },
        };
        let provider = match self.coverage.provider_at(height) {
            Ok(Some(provider @ (Provider::Store | Provider::Head))) => provider,
            _ => return Ok(None),
        };
        let Some(hash) = self.pinned_hash(provider, height).await? else {
            return Ok(None);
        };
        if matches!(at, BlockId::Hash(asked) if asked != hash) {
            return Ok(None);
        }
        let (time, roots) = match provider {
            Provider::Head => {
                let block = self.head_block(height).ok_or_else(|| unpinned(height))?;
                (block.block.header.time, block.tree_roots.clone())
            }
            _ => {
                let header = self
                    .reader
                    .headers_chunk(height, height)
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| unpinned(height))?;
                let roots = self
                    .reader
                    .tree_roots_chunk(height, height)
                    .await
                    .map_err(store_err)?
                    .into_iter()
                    .next()
                    .ok_or_else(|| unpinned(height))?;
                (header.header.time, roots)
            }
        };
        let mut pools = Vec::with_capacity(ShieldedPool::ALL.len());
        for (pool, root) in
            ShieldedPool::ALL
                .into_iter()
                .zip([roots.sapling, roots.orchard, roots.ironwood])
        {
            let frontier = match provider {
                Provider::Head => self.head.pool_frontier(pool, height).map_err(head_err)?,
                _ => self
                    .reader
                    .pool_frontier(pool, height)
                    .await
                    .map_err(store_err)?,
            };
            let Some(frontier) = frontier else {
                return Ok(None);
            };
            pools.push(pool_treestate(frontier, root)?);
        }
        let [sapling, orchard, ironwood]: [Option<PoolTreestate>; 3] = pools
            .try_into()
            .map_err(|_| ChainViewError::Fatal("three pools expected".to_owned()))?;
        Ok(Some(Treestate {
            block_hash: hash,
            height,
            time,
            sapling,
            orchard,
            ironwood,
        }))
    }

    /// `pool`'s subtree roots from `start_index`: the store's, completed at
    /// or below its tip, then the head's above it. `None` sends the read to
    /// the validator: the trees not built locally, or the pool not active at
    /// the tip (the validator's refusal stays its own).
    async fn local_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: SubtreeIndex,
        limit: Option<u16>,
    ) -> Result<Option<Vec<SubtreeRoot>>> {
        if !self.local_trees() {
            return Ok(None);
        }
        let tip = self.head.best_tip().height;
        match self.head.pool_frontier(pool, tip).map_err(head_err)? {
            None | Some(PoolFrontier::Absent) => return Ok(None),
            Some(_) => {}
        }
        let limit = limit.map_or(u64::MAX, u64::from);
        let mut roots = match self.coverage.store_top {
            Some(_) => self
                .reader
                .subtree_roots(pool, u64::from(start_index), limit)
                .await
                .map_err(store_err)?,
            None => Vec::new(),
        };
        let taken = u64::try_from(roots.len())
            .map_err(|_| ChainViewError::Fatal("subtree count overflowed".to_owned()))?;
        if let Some(remaining) = limit.checked_sub(taken).filter(|left| *left > 0) {
            let next = u64::from(start_index).saturating_add(taken);
            roots.extend(
                self.head
                    .subtree_roots(pool, next, remaining)
                    .map_err(head_err)?,
            );
        }
        Ok(Some(roots))
    }
}

/// Whether both halves build a capability: the store (enabled, with the
/// index) to its tip and the head above it. Shared by the manifest and the
/// reads, so what is advertised is what is answered.
fn both_build(
    config: &ChainViewConfig,
    store_capabilities: zaino_chain_store::StoreCapabilities,
    store: StoreCapability,
    head_capabilities: HeadCapabilities,
    head: HeadCapability,
) -> bool {
    config.store_enabled && store_capabilities.contains(store) && head_capabilities.contains(head)
}

/// One pool's treestate from its frontier and root; `None` for a pool not
/// active at the block.
fn pool_treestate(
    frontier: PoolFrontier,
    root: Option<zaino_primitives::types::TreeRootInfo>,
) -> Result<Option<PoolTreestate>> {
    let tree = match &frontier {
        PoolFrontier::Absent => return Ok(None),
        PoolFrontier::Empty => None,
        PoolFrontier::Frontier(frontier) => Some(frontier),
    };
    let final_state = legacy_tree_from_frontier(tree)
        .map_err(|error| ChainViewError::Fatal(format!("a stored frontier: {error}")))?;
    Ok(Some(PoolTreestate {
        final_root: root.map(|info| info.root),
        final_state,
    }))
}

/// A pool's treestate with its root filled in.
fn with_root(
    pool: Option<zaino_primitives::types::PoolTreestate>,
    root: Option<zaino_primitives::types::TreeRootInfo>,
) -> Option<zaino_primitives::types::PoolTreestate> {
    pool.map(|pool| zaino_primitives::types::PoolTreestate {
        final_root: root.map(|info| info.root),
        ..pool
    })
}

// ***** Optional capabilities *****
//
// Each is implemented only where its providers can supply it, so a deployment
// whose store lacks the backing index gets a view that genuinely does not have
// the trait — checked by the compiler rather than reported at runtime.

impl<Reader, HeadSnapshot, Source> SpendRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + SpentOutputIndex,
    HeadSnapshot: ChainHeadSnapshot + ChainHeadTransactionService,
    Source: ChainViewSource,
{
    async fn outpoint_spenders(
        &self,
        outpoints: &[Outpoint],
        scope: ChainScope,
    ) -> Result<Vec<SpendStatus>> {
        if !self.served.contains(ChainCapability::SpendStatus) {
            return Err(withheld(ChainCapability::SpendStatus));
        }
        if self.coverage.store_top.is_none() {
            return Err(ChainViewError::NotServiceable(
                "spend status needs the finalised store, which is disabled",
            ));
        }

        let finalised = self
            .reader
            .outpoint_spenders(outpoints)
            .await
            .map_err(store_err)?;

        if scope == ChainScope::Finalised {
            return Ok(finalised
                .into_iter()
                .map(|spender| match spender {
                    Some(spender) => stored_spender(spender),
                    None => SpendStatus::Unspent,
                })
                .collect());
        }

        // The validator runs no spend index, so a hole between the providers
        // cannot be filled and a full-chain answer spanning one would be
        // silently incomplete. Unlike a block read there is nothing to degrade
        // to, so refusing is the only honest option.
        if !self.coverage.contiguous() {
            return Err(ChainViewError::NotServiceable(
                "spend status cannot span the range the finalised store has not built",
            ));
        }

        // The recent window wins where it has an answer: an outpoint is spent
        // at most once on the best chain, so the two cannot disagree — but they
        // can be at different points in time, and the window is the later one.
        Ok(self
            .head
            .outpoint_spenders(outpoints)
            .map_err(head_err)?
            .into_iter()
            .zip(finalised)
            .map(|(recent, finalised)| match (recent, finalised) {
                (Some(spender), _) => SpendStatus::SpentBy {
                    txid: spender.txid,
                    input_index: spender.input_index,
                    height: spender.block.height,
                },
                (None, Some(spender)) => stored_spender(spender),
                (None, None) => SpendStatus::Unspent,
            })
            .collect())
    }
}

impl<Reader, HeadSnapshot, Source> TxOutSetRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TxOutSetIndex + SpentOutputIndex + TransactionIndex,
    HeadSnapshot: ChainHeadTxOutSetService,
    Source: ChainViewSource,
{
    /// The head's set at the tip: the store's set at its tip extended by the
    /// head's `txout_set` extension, so there is nothing to merge here.
    async fn txout_set(&self) -> Result<TxOutSetAccumulator> {
        if !self.served.contains(ChainCapability::TxOutSet) {
            return Err(withheld(ChainCapability::TxOutSet));
        }
        if !self.config.store_enabled {
            return Err(ChainViewError::NotServiceable(
                "the txout set starts from the finalised store, which is disabled",
            ));
        }
        self.head.txout_set().map_err(head_err)
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Source: ChainViewSource,
{
    /// The shape every transparent-address read shares.
    ///
    /// Each one refuses the same two ways before it asks anything — the
    /// deployment may withhold the capability, and the validator may be
    /// disabled. Only the port call differs, so it arrives as the future it
    /// returns and the rest is written once.
    ///
    /// An address never paid is an ordinary answer from these ports, so every
    /// domain error they raise is an invalid request.
    ///
    /// A caller builds that future before the guards run, which costs nothing:
    /// an `async fn` does no work until awaited, so a withheld capability still
    /// never reaches the validator.
    async fn address_query<T, E, N>(
        &self,
        call: impl core::future::Future<
            Output = core::result::Result<T, zaino_source::QueryError<E, N>>,
        >,
    ) -> Result<T>
    where
        E: core::fmt::Debug + core::fmt::Display,
        N: std::error::Error + Into<zaino_source::NonDomainError>,
    {
        if !self.served.contains(ChainCapability::AddressHistory) {
            return Err(withheld(ChainCapability::AddressHistory));
        }
        self.fetch
            .require("address history needs the validator, which is disabled")?;
        fetch::answered(call.await)
    }
}

impl<Reader, HeadSnapshot, Source> AddressRead for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TransparentHistoryIndex,
    HeadSnapshot: ChainHeadTransparentHistoryService,
    Source: ChainViewSource,
{
    // The store's effects to its tip and the head's above it where both build
    // address history with nothing between them, shaped as the validator
    // shapes them; else the validator's answers.

    async fn address_balance(&self, addresses: &[TransparentAddress]) -> Result<AddressBalance> {
        let Some(effects) = self.local_effects(addresses, None).await? else {
            return self
                .address_query(self.fetch.source().get_address_balance(encoded(addresses)))
                .await;
        };
        let unspent = effects.unspent();
        Ok(AddressBalance {
            balance: Zatoshis::sum_balances(unspent.iter().map(|effect| effect.value))
                .ok_or_else(|| amount_err("unspent outputs past the supply"))?,
            received: ZatoshisFlowSum::try_accumulate(
                effects
                    .all
                    .iter()
                    .filter(|effect| effect.credit.is_some())
                    .map(|effect| effect.value),
            )
            .ok_or_else(|| amount_err("received flow overflowed"))?,
        })
    }

    async fn address_utxos(&self, addresses: &[TransparentAddress]) -> Result<Vec<Utxo>> {
        let Some(effects) = self.local_effects(addresses, None).await? else {
            return self
                .address_query(self.fetch.source().get_address_utxos(encoded(addresses)))
                .await;
        };
        let unspent = effects.unspent();
        capped(unspent.len())?;
        Ok(unspent
            .into_iter()
            .filter_map(|effect| {
                let script = effect.credit.clone()?;
                Some(Utxo {
                    address: effect.address.clone(),
                    txid: effect.txid,
                    output_index: effect.index,
                    script,
                    satoshis: effect.value,
                    height: effect.height,
                })
            })
            .collect())
    }

    async fn address_txids(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>> {
        let range = Some((start, end, RangeRule::WithinTip));
        let Some(effects) = self.local_effects(addresses, range).await? else {
            return self
                .address_query(self.fetch.source().get_address_txids(
                    encoded(addresses),
                    start,
                    end,
                ))
                .await;
        };
        let txids = effects.transactions();
        capped(txids.len())?;
        Ok(txids)
    }

    async fn address_deltas(
        &self,
        addresses: &[TransparentAddress],
        start: Height,
        end: Height,
    ) -> Result<Vec<AddressDelta>> {
        let range = Some((start, end, RangeRule::Ordered));
        let Some(effects) = self.local_effects(addresses, range).await? else {
            return self
                .address_query(self.fetch.source().get_address_deltas(
                    encoded(addresses),
                    start,
                    end,
                ))
                .await;
        };
        capped(effects.transactions().len())?;
        effects
            .all
            .iter()
            .map(|effect| {
                let value = i64::try_from(u64::from(effect.value))
                    .map_err(|_| amount_err("a value past i64"))?;
                let signed = match effect.credit {
                    Some(_) => Some(value),
                    None => value.checked_neg(),
                }
                .ok_or_else(|| amount_err("a debit past i64"))?;
                Ok(AddressDelta {
                    satoshis: SignedZatoshis::try_new(signed)
                        .map_err(|error| amount_err(&error.to_string()))?,
                    txid: effect.txid,
                    index: effect.index,
                    height: effect.height,
                    address: effect.address.clone(),
                    block_index: Some(effect.position),
                })
            })
            .collect()
    }
}

/// One effect on a queried address: an output paying it (`credit`, with the
/// output's script) or an input spending such an output.
struct AddressEffect {
    address: TransparentAddress,
    height: Height,
    position: u32,
    txid: TransactionId,
    /// The output index for a credit, the input index for a debit.
    index: u32,
    value: Zatoshis,
    /// The output's script for a credit; `None` for a debit.
    credit: Option<zaino_primitives::types::Script>,
    /// The outpoint created (credit) or spent (debit).
    outpoint: Outpoint,
}

/// The effects of one query, in the validator's order: by height, position
/// and index, a credit before a debit at the same index.
struct AddressEffects {
    all: Vec<AddressEffect>,
}

impl AddressEffects {
    /// The credits no debit spends, in order.
    fn unspent(&self) -> Vec<&AddressEffect> {
        let spent: std::collections::HashSet<Outpoint> = self
            .all
            .iter()
            .filter(|effect| effect.credit.is_none())
            .map(|effect| effect.outpoint)
            .collect();
        self.all
            .iter()
            .filter(|effect| effect.credit.is_some() && !spent.contains(&effect.outpoint))
            .collect()
    }

    /// Each transaction touching the addresses once, in order.
    fn transactions(&self) -> Vec<TransactionId> {
        let mut seen = std::collections::HashSet::new();
        self.all
            .iter()
            .map(|effect| effect.txid)
            .filter(|txid| seen.insert(*txid))
            .collect()
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + TransparentHistoryIndex,
    HeadSnapshot: ChainHeadTransparentHistoryService,
    Source: ChainViewSource,
{
    /// Every effect on `addresses` over `range` (`None`: the whole chain):
    /// the store's to its tip, the head's above it. `None` sends the read to
    /// the validator: address history not built locally. A withheld
    /// capability and an address for another network are refused here, as
    /// the validator refuses them.
    async fn local_effects(
        &self,
        addresses: &[TransparentAddress],
        range: Option<(Height, Height, RangeRule)>,
    ) -> Result<Option<AddressEffects>> {
        if !self.served.contains(ChainCapability::AddressHistory) {
            return Err(withheld(ChainCapability::AddressHistory));
        }
        let local = both_build(
            &self.config,
            self.reader.capabilities(),
            StoreCapability::TransparentHistory,
            self.head.capabilities(),
            HeadCapability::TransparentHistory,
        ) && self.coverage.contiguous();
        if !local {
            return Ok(None);
        }
        let tip = self.head.best_tip().height;
        if let Some((start, end, rule)) = range {
            let past_tip = matches!(rule, RangeRule::WithinTip) && (start > tip || end > tip);
            if start > end || past_tip {
                return Err(invalid_range(start, end));
            }
        }
        let network = self.reader.network();
        if let Some(foreign) = addresses.iter().find(|address| !address.is_for(network)) {
            return Err(ChainViewError::Rejected(format!(
                "an address for another network: {foreign}"
            )));
        }
        let by_key: HashMap<TransparentAddressKey, TransparentAddress> = addresses
            .iter()
            .map(|address| (address.key(), address.clone()))
            .collect();
        let keys: Vec<TransparentAddressKey> = by_key.keys().copied().collect();
        let (start, end) = range.map_or((Height::GENESIS, tip), |(start, end, _)| (start, end));
        let end = end.min(tip);
        let mut all = Vec::new();
        if start > end {
            return Ok(Some(AddressEffects { all }));
        }
        let named = |key: &TransparentAddressKey| {
            by_key
                .get(key)
                .cloned()
                .ok_or_else(|| ChainViewError::Fatal("an effect for an unasked address".to_owned()))
        };

        // the store to its tip
        if let Some(store_top) = self.coverage.store_top.filter(|top| *top >= start) {
            let stored = self
                .reader
                .address_effects(&zaino_chain_store::TransparentHistoryQuery {
                    addresses: keys.clone(),
                    start,
                    end: end.min(store_top),
                })
                .await
                .map_err(store_err)?;
            for output in stored.outputs {
                all.push(AddressEffect {
                    address: named(&output.address)?,
                    height: output.position.height,
                    position: output.position.tx_index,
                    txid: output.txid,
                    index: output.outpoint.index,
                    value: output.output.value,
                    credit: Some(output.output.script),
                    outpoint: output.outpoint,
                });
            }
            for spend in stored.spends {
                all.push(AddressEffect {
                    address: named(&spend.address)?,
                    height: spend.position.height,
                    position: spend.position.tx_index,
                    txid: spend.txid,
                    index: spend.input_index,
                    value: spend.value,
                    credit: None,
                    outpoint: spend.outpoint,
                });
            }
        }

        // the head above it
        let above = self
            .coverage
            .store_top
            .and_then(|top| top.checked_add(1))
            .unwrap_or(Height::GENESIS)
            .max(start);
        if above <= end {
            let recent = self
                .head
                .address_effects(&zaino_chain_head::transparent::TransparentHistoryQuery {
                    addresses: keys,
                    start: above,
                    end,
                })
                .map_err(head_err)?;
            for output in recent
                .outputs
                .into_iter()
                .filter(|o| o.block.height >= above)
            {
                all.push(AddressEffect {
                    address: named(&output.address)?,
                    height: output.block.height,
                    position: output.tx_index,
                    txid: output.outpoint.txid,
                    index: output.outpoint.index,
                    value: output.value,
                    credit: Some(output.script),
                    outpoint: output.outpoint,
                });
            }
            for spend in recent
                .spends
                .into_iter()
                .filter(|s| s.block.height >= above)
            {
                all.push(AddressEffect {
                    address: named(&spend.address)?,
                    height: spend.block.height,
                    position: spend.tx_index,
                    txid: spend.txid,
                    index: spend.input_index,
                    value: spend.value,
                    credit: None,
                    outpoint: spend.outpoint,
                });
            }
        }
        all.sort_by_key(|effect| {
            (
                effect.height,
                effect.position,
                effect.index,
                effect.credit.is_none(),
            )
        });
        Ok(Some(AddressEffects { all }))
    }
}

// ***** The chain history tree *****

impl<Reader, HeadSnapshot, Source> HistoryTreeRead
    for ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader + HistoryTreeIndex,
    HeadSnapshot: ChainHeadHistoryTreeService,
    Source: ChainViewSource,
{
    async fn history_root(&self, height: Height) -> Result<Option<[u8; 32]>> {
        match self.history_provider(height)? {
            None => Ok(None),
            Some(Provider::Store) => self.reader.history_root(height).await.map_err(store_err),
            Some(_) => self.head.history_root(height).map_err(head_err),
        }
    }

    async fn history_epoch(&self, height: Height) -> Result<Option<HistoryEpoch>> {
        match self.history_provider(height)? {
            None => Ok(None),
            Some(Provider::Store) => self.reader.history_epoch(height).await.map_err(store_err),
            Some(_) => Ok(self
                .head
                .history_epoch(height)
                .map_err(head_err)?
                .map(|epoch| HistoryEpoch {
                    branch: epoch.branch,
                    length: epoch.length,
                })),
        }
    }

    async fn history_entries(
        &self,
        height: Height,
        positions: &[u32],
    ) -> Result<Vec<Option<Vec<u8>>>> {
        let provider = match self.history_provider(height)? {
            None => return Ok(vec![None; positions.len()]),
            Some(provider) => provider,
        };
        if provider == Provider::Store {
            return self
                .reader
                .history_entries(height, positions)
                .await
                .map_err(store_err);
        }
        // the head holds what its window appended; positions below its seed
        // are the store's, in the same epoch (an MMR entry never changes)
        let mut entries = self
            .head
            .history_entries(height, positions)
            .map_err(head_err)?;
        let (Some(store_top), Some(epoch)) = (
            self.coverage.store_top,
            self.head.history_epoch(height).map_err(head_err)?,
        ) else {
            return Ok(entries);
        };
        let same_epoch = self
            .reader
            .history_epoch(store_top)
            .await
            .map_err(store_err)?
            .is_some_and(|stored| stored.branch == epoch.branch);
        if !same_epoch {
            return Ok(entries);
        }
        let missing: Vec<u32> = positions
            .iter()
            .zip(&entries)
            .filter(|(position, entry)| entry.is_none() && **position < epoch.length)
            .map(|(position, _)| *position)
            .collect();
        let stored = self
            .reader
            .history_entries(store_top, &missing)
            .await
            .map_err(store_err)?;
        let mut stored = missing.into_iter().zip(stored);
        for (position, entry) in positions.iter().zip(entries.iter_mut()) {
            if entry.is_none() && *position < epoch.length {
                if let Some((_, filled)) = stored.next() {
                    *entry = filled;
                }
            }
        }
        Ok(entries)
    }
}

impl<Reader, HeadSnapshot, Source> ComposerSnapshot<Reader, HeadSnapshot, Source>
where
    Reader: ChainStoreReader,
    HeadSnapshot: ChainHeadSnapshot,
{
    /// Who answers the history tree at `height`: the store at or below its
    /// tip, the head above it; `None` above the tip. Refused where the two
    /// do not both build it or something lies between them.
    fn history_provider(&self, height: Height) -> Result<Option<Provider>> {
        if !self.served.contains(ChainCapability::HistoryTree) {
            return Err(withheld(ChainCapability::HistoryTree));
        }
        let local = both_build(
            &self.config,
            self.reader.capabilities(),
            StoreCapability::HistoryTree,
            self.head.capabilities(),
            HeadCapability::HistoryTree,
        ) && self.coverage.contiguous();
        if !local {
            return Err(ChainViewError::NotServiceable(
                "the history tree needs the store's and the chain head's, with nothing between",
            ));
        }
        if height > self.head.best_tip().height {
            return Ok(None);
        }
        Ok(Some(match self.coverage.store_top {
            Some(top) if height <= top => Provider::Store,
            _ => Provider::Head,
        }))
    }
}

/// The store's spender as a spend status.
fn stored_spender(spender: zaino_chain_store::SpenderRef) -> SpendStatus {
    SpendStatus::SpentBy {
        txid: spender.txid,
        input_index: spender.input_index,
        height: spender.position.height,
    }
}

/// How the validator checks a queried range before answering.
#[derive(Clone, Copy)]
enum RangeRule {
    /// Refused when inverted.
    Ordered,
    /// Refused when inverted or past the tip.
    WithinTip,
}

/// An answer over the validator's result limit is refused, as it refuses it.
fn capped(rows: usize) -> Result<()> {
    match rows > zaino_source::MAX_ADDRESS_QUERY_RESULTS {
        true => Err(ChainViewError::Rejected(format!(
            "more than {} results",
            zaino_source::MAX_ADDRESS_QUERY_RESULTS
        ))),
        false => Ok(()),
    }
}

/// A range the validator refuses: inverted, or (for txids) past the tip.
fn invalid_range(start: Height, end: Height) -> ChainViewError {
    ChainViewError::Rejected(format!("invalid range {start}..={end}"))
}

/// Amounts that left their range: the store and head disagree with the chain.
fn amount_err(what: &str) -> ChainViewError {
    ChainViewError::Fatal(what.to_owned())
}

/// Addresses as the source ports name them.
///
/// The ports take encoded strings because that is what the validator's RPC
/// takes; this crate's surface takes the domain type, so the encoding happens
/// once here rather than at every call site.
fn encoded(addresses: &[TransparentAddress]) -> Vec<String> {
    addresses
        .iter()
        .map(|address| address.as_str().to_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A capability the head does not build is not serviceable; a built one
    /// that fails to read is a fault, never a reason to fall back.
    #[test]
    fn head_errors_keep_not_built_apart_from_faults() {
        assert!(matches!(
            head_err(ChainHeadError::NotBuilt {
                capability: "tree state"
            }),
            ChainViewError::NotServiceable(_)
        ));
        assert!(matches!(
            head_err(ChainHeadError::ReadFailed {
                capability: "transparent history",
                reason: "overlay".to_owned(),
            }),
            ChainViewError::Fatal(_)
        ));
    }

    /// The store moves 40 blocks and the head rebases onto it between the
    /// two reads of one snapshot: read head first, the reader's tip is still
    /// at or above the head's starting block, so the address answers lose
    /// nothing and equal the validator's.
    // multi_thread required: the hook blocks in place on the store's build
    #[tokio::test(flavor = "multi_thread")]
    async fn a_store_moving_between_the_reads_leaves_no_gap() {
        use crate::common::{chain, height, rig, TIP};
        use zaino_primitives::types::Network;

        let store_top = TIP - 50;
        let local = rig(chain())
            .store_to(store_top)
            .extended_head()
            .build()
            .await;
        let (store, head) = (local.store.clone(), Arc::clone(&local.head));
        let view = local.view().between_reads(move || {
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    store
                        .build_to(height(store_top + 40))
                        .await
                        .expect("builds");
                    head.advance_once().await.expect("rebases");
                })
            })
        });
        let snapshot = view.snapshot();
        let reference = rig(chain()).store_to(store_top).build().await;
        let expected = reference.view().snapshot();
        let miner = vec![TransparentAddress::try_new(
            zaino_source_mock::address_of(&zaino_source_mock::MINER_SCRIPT, Network::Regtest)
                .expect("standard"),
        )
        .expect("valid")];
        let (start, end) = (height(0), height(TIP));
        assert_eq!(
            snapshot
                .address_deltas(&miner, start, end)
                .await
                .expect("answered"),
            expected
                .address_deltas(&miner, start, end)
                .await
                .expect("answered")
        );
        assert_eq!(
            snapshot.address_balance(&miner).await.expect("answered"),
            expected.address_balance(&miner).await.expect("answered")
        );
    }
}
