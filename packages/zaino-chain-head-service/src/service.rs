//! The ChainHead runtime.
//!
//! The graph advancement, reorg handling and retention below are the
//! non-finalised state's, moved here and changed only where the new boundaries
//! force it:
//!
//! - the finalised state is gone. The old `sync` took an `Arc<FinalisedState>`
//!   and read `db_height()` for both its anchor floor and its trim floor; both
//!   now come from the chain tip and the configured depth, which is the arm the
//!   old code already took whenever the database lagged.
//! - the source is the `zaino-source` ports rather than the wire-typed
//!   scaffolding, so `get_block(HashOrHeight)` splits into the by-height and
//!   by-hash questions it always was underneath.
//! - blocks are [`ChainHeadBlock`] rather than `IndexedBlock`, so the graph
//!   holds no persistence type.
//! - the runtime owns the loop that used to live in ChainIndex's sync worker.
//!
//! The block-carrying listener and `add_nonbest_block` are not here: no source
//! ever implemented `nonfinalized_listener`, so both were unreachable.
//!
//! # Who picks the tip
//!
//! The heaviest retained chain; on equal work, the validator's tip. A
//! validator tip lighter than a retained branch is reported in the status and
//! counted; see the crate's `usage.md`.
//!
//! # Advancing is not an operation
//!
//! There is no `sync`, `update` or `reconcile` here at any visibility. The
//! writer task is the only thing that advances the graph, and it does so
//! through private methods that build a *new* snapshot and hand it to
//! [`publish_snapshot`](ChainHeadService::publish_snapshot). Nothing else can
//! reach the published cell, so a reader can never observe a half-applied
//! reorg or a partially-extended window.

use std::{
    fmt,
    sync::{Arc, Mutex, PoisonError},
    time::Duration,
};

use arc_swap::ArcSwap;
use futures::StreamExt as _;
use tokio::{sync::watch, task::JoinHandle};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, instrument, warn};
use zaino_chain_head::{
    ChainHeadBlock, ChainHeadBlockSource, ChainHeadConfig, ChainHeadSnapshot as _,
};
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, StatusCell, StatusSource,
};
use zaino_primitives::types::{
    BlockHash, BlockRef, ChainStateEpoch, Height, RelativeChainWork, TreeRoots,
};

use crate::{
    error::{ChainHeadAdvanceError, ChainHeadInitError},
    graph::{ChainGraph as _, NotChildOfTip, NotOnBestChain},
    snapshot::PersistentSnapshot,
    subscriber::ChainHeadSubscriber,
};

/// The name this component reports status under.
const COMPONENT: &str = "ChainHead";

/// Retention margin below the configured depth.
///
/// Trimming stops this far below the seam, so a block stays readable in the
/// graph for a while after it has been handed off as final. It also bounds the
/// reorg ancestry walk, which may not step further back than the window the
/// graph maintains.
///
/// It does not guard the handoff. Every block that crosses the seam is handed
/// off whatever the margin is, because trimming runs after the handoff has read
/// them. Nor does it widen the reorg-possible range: `MAX_NONFINALISED_DEPTH` is
/// already the reorg bound plus the fork point, so the window covers the deepest
/// reorg it is sized for without this.
const RETENTION_MARGIN: u32 = 10;

/// The validator's tip and the heavier retained branch the head follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TipDisagreement {
    validator: BlockRef,
    heavier: BlockRef,
}

/// The bounded non-finalised head of the chain, kept current with a validator.
///
/// Owns exactly one writer task. Everything else holds a
/// [`ChainHeadSubscriber`], which reads published snapshots and nothing else.
pub struct ChainHeadService<S: ChainHeadBlockSource> {
    /// We need access to the validator's best block hash, as well as a source
    /// of blocks.
    source: Arc<S>,
    /// This lock should not be exposed to consumers. Rather, clone the Arc and
    /// offer that. This means we can overwrite the arc without interfering with
    /// readers, who will hold a stale copy.
    current: Arc<ArcSwap<PersistentSnapshot>>,
    updates: watch::Sender<ChainStateEpoch>,
    status: StatusCell,
    cancel: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
    config: ChainHeadConfig,
    /// Whether the validator's side-chain tips have been laid down (HEAD-2).
    seeded: std::sync::atomic::AtomicBool,
    /// The extensions the builder enabled (D-50), with their ports.
    #[cfg(any(
        feature = "transactions",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    extensions: Option<crate::extension::Enabled>,
    /// Whether the extensions' first fold has been started: the blocks are
    /// published, and `Ready`, before it.
    #[cfg(any(
        feature = "transactions",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    folding: std::sync::atomic::AtomicBool,
}

impl<S: ChainHeadBlockSource> std::fmt::Debug for ChainHeadService<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainHeadService")
            .field("status", &self.status.status())
            .field("best_tip", &self.current.load().best_tip())
            .finish_non_exhaustive()
    }
}

impl<S: ChainHeadBlockSource> ChainHeadService<S> {
    /// Anchors the graph, then starts the writer task that extends it.
    ///
    /// Anchoring is the old `initialize`, through `anchor_block`: one block
    /// at the anchor height, which the writer task then extends one block at a
    /// time. Doing it before returning is what makes
    /// `ChainHeadSubscriber::current` total — there is no state in which a
    /// ChainHead exists with nothing to answer from.
    ///
    /// # Shutdown contract
    ///
    /// **Dropping the returned `Arc` does not stop the writer task.** The task
    /// holds its own `Arc<Self>`, so the service outlives every handle a caller
    /// keeps. Stop it by cancelling `cancel` or by calling
    /// [`shutdown`](Self::shutdown); a caller that does neither leaks the task
    /// for the life of the process.
    ///
    /// This is deliberate rather than an oversight. A writer that stopped when
    /// the last read handle went away would stop mid-request in any consumer
    /// that briefly holds no subscriber, and the task must outlive its handles
    /// to publish at all. The cost is that the caller owns the lifetime, so
    /// pass a token that is actually cancelled — see the cancellation section
    /// of this crate's `usage.md` for why it should be a *child* token.
    #[instrument(name = "ChainHeadService::spawn", skip_all, fields(max_depth = config.max_depth()))]
    pub async fn spawn(
        source: Arc<S>,
        config: ChainHeadConfig,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ChainHeadInitError> {
        Self::builder(source, config, cancel).spawn().await
    }

    /// A service built step by step: the extensions it folds (D-50), then
    /// [`spawn`](ChainHeadBuilder::spawn).
    pub fn builder(
        source: Arc<S>,
        config: ChainHeadConfig,
        cancel: CancellationToken,
    ) -> ChainHeadBuilder<S> {
        ChainHeadBuilder {
            source,
            config,
            cancel,
            #[cfg(any(
                feature = "transactions",
                feature = "tree_state",
                feature = "history_tree"
            ))]
            extensions: None,
        }
    }

    /// Anchors, then starts the writer task.
    async fn started(builder: ChainHeadBuilder<S>) -> Result<Arc<Self>, ChainHeadInitError> {
        let service = Self::anchored(builder).await?;

        let worker = Arc::clone(&service);
        let handle = tokio::spawn(async move { worker.run().await });
        *service.task.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);

        Ok(service)
    }

    /// An anchored service with **no writer task**, for tests that step it.
    ///
    /// Compiled out of production builds. Pair with
    /// [`advance_once`](Self::advance_once): with no writer running, a stepping
    /// test is the only thing advancing the graph, so what it observes is
    /// exactly what it caused.
    ///
    /// Shares `anchored` with [`spawn`](Self::spawn), so the
    /// two construction paths cannot drift — they differ only in whether the
    /// task is started.
    #[cfg(any(test, feature = "testing"))]
    pub async fn spawn_without_writer(
        source: Arc<S>,
        config: ChainHeadConfig,
        cancel: CancellationToken,
    ) -> Result<Arc<Self>, ChainHeadInitError> {
        Self::builder(source, config, cancel)
            .spawn_without_writer()
            .await
    }

    /// Advances the graph by one iteration and publishes the result.
    ///
    /// Compiled out of production builds. This is what the writer task does per
    /// tick; exposing it to tests lets them assert on a specific reorg shape
    /// without racing a timer.
    #[cfg(any(test, feature = "testing"))]
    pub async fn advance_once(&self) -> Result<(), ChainHeadAdvanceError> {
        self.tick().await
    }

    /// Everything [`spawn`](Self::spawn) does except start the task.
    async fn anchored(builder: ChainHeadBuilder<S>) -> Result<Arc<Self>, ChainHeadInitError> {
        let ChainHeadBuilder {
            source,
            config,
            cancel,
            #[cfg(any(
                feature = "transactions",
                feature = "tree_state",
                feature = "history_tree"
            ))]
            extensions,
        } = builder;
        let status = StatusCell::new(
            ComponentName(COMPONENT),
            Lifecycle::Syncing,
            Health::Healthy,
        );

        let snapshot = anchor_with_retry(&source, &config, &cancel).await?;
        info!(
            height = u32::from(snapshot.best_tip().height),
            hash = %snapshot.best_tip().hash,
            "ChainHead anchored"
        );

        let (updates, _) = watch::channel(ChainStateEpoch {
            generation: 0,
            best_tip: snapshot.best_tip(),
        });
        let service = Arc::new(Self {
            source,
            current: Arc::new(ArcSwap::from_pointee(snapshot)),
            updates,
            status,
            cancel,
            task: Mutex::new(None),
            config,
            seeded: std::sync::atomic::AtomicBool::new(false),
            #[cfg(any(
                feature = "transactions",
                feature = "tree_state",
                feature = "history_tree"
            ))]
            extensions,
            #[cfg(any(
                feature = "transactions",
                feature = "tree_state",
                feature = "history_tree"
            ))]
            folding: std::sync::atomic::AtomicBool::new(false),
        });
        // Still `Syncing`: the anchor is the window's floor, not its tip, so a
        // reader served now would see a head up to `max_depth` below the
        // chain. `Ready` is published by the first successful advance, which
        // is the first moment the snapshot matches the validator's tip.

        Ok(service)
    }

    /// A read-only handle onto the published snapshot.
    ///
    /// The status cell is cloned, not read: the handle observes every later
    /// transition rather than the value that happened to hold here.
    pub fn subscriber(&self) -> ChainHeadSubscriber {
        ChainHeadSubscriber::new(
            Arc::clone(&self.current),
            self.updates.subscribe(),
            self.status.clone(),
        )
    }

    /// The runtime's current status.
    pub fn status(&self) -> ComponentStatus {
        self.status.status()
    }

    /// Stops the writer task.
    ///
    /// The cancellation token passed to [`spawn`](Self::spawn) also stops the
    /// task; this additionally publishes `Closing` and releases the handle, so
    /// shutdown is observable rather than merely effective.
    ///
    /// Synchronous, and does **not** wait for the task to wind down: it cancels
    /// and then aborts. It cannot wait, because it is called from `Drop`. The
    /// abort is safe rather than merely expedient — a snapshot is installed with
    /// one atomic store, so a task killed part-way through building a candidate
    /// leaves the last published snapshot whole. The status is stored before the
    /// abort so `Closing` is observable on every handle regardless of when the
    /// task dies.
    pub fn shutdown(&self) {
        self.status.set(Lifecycle::Closing, Health::Healthy);
        self.cancel.cancel();
        // a poisoned lock still holds the handle: abort it all the same
        if let Some(handle) = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }

    /// The writer task.
    ///
    /// The loop ChainIndex's sync worker ran for the non-finalised state, with
    /// the same backoff ladder and the same escalation to `CriticalError` after
    /// a run of failures.
    async fn run(self: Arc<Self>) {
        let mut wake = self.source.subscribe_to_blocks_received();
        let mut backoff = self.config.initial_backoff();
        let mut consecutive_failures = 0u32;

        loop {
            if self.cancel.is_cancelled() {
                break;
            }

            let iteration = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => break,
                result = self.tick() => result,
            };

            match iteration {
                Ok(()) => {
                    consecutive_failures = 0;
                    backoff = self.config.initial_backoff();
                    record_sync_failures(0, Duration::ZERO);
                    // `Ready` is already published from inside `tick`, before
                    // the advanced snapshot becomes observable to readers.
                    if self.wait_for_work(&mut wake).await.is_break() {
                        break;
                    }
                }
                Err(error) => {
                    consecutive_failures = consecutive_failures.saturating_add(1);
                    record_sync_failures(consecutive_failures, backoff);
                    if consecutive_failures >= self.config.max_consecutive_failures() {
                        warn!(
                            %error,
                            attempts = consecutive_failures,
                            "ChainHead giving up on the validator; last published snapshot is now stale",
                        );
                        record(&self.status, TickOutcome::GaveUp);
                        break;
                    }
                    warn!(%error, attempts = consecutive_failures, "ChainHead failed to advance; retrying");
                    record(&self.status, TickOutcome::Retrying);
                    if sleep_or_cancel(backoff, &self.cancel).await.is_break() {
                        break;
                    }
                    backoff = backoff.saturating_mul(2).min(self.config.max_backoff());
                }
            }
        }

        debug!("ChainHead writer task stopped");
    }

    /// Waits for the poll interval, or for the source to say it has new blocks.
    ///
    /// The wake is a latency hint and nothing more: it carries no payload, and
    /// the next iteration re-reads the source regardless.
    async fn wait_for_work(
        &self,
        wake: &mut Option<watch::Receiver<()>>,
    ) -> std::ops::ControlFlow<()> {
        match wake {
            Some(rx) => tokio::select! {
                _ = self.cancel.cancelled() => std::ops::ControlFlow::Break(()),
                _ = tokio::time::sleep(self.config.poll_interval()) => std::ops::ControlFlow::Continue(()),
                changed = rx.changed() => {
                    if changed.is_err() {
                        // The source dropped its sender. Fall back to the
                        // interval for the rest of this runtime's life.
                        *wake = None;
                    }
                    std::ops::ControlFlow::Continue(())
                }
            },
            None => sleep_or_cancel(self.config.poll_interval(), &self.cancel).await,
        }
    }

    /// One iteration: read the tip, build the next graph, publish it.
    ///
    /// The old `sync`, less the publishing. `chain_height` is read once and the
    /// build is bounded by it, so a source advance mid-iteration — the
    /// validator producing blocks while this is still running — is deferred to
    /// the next iteration, which reads a fresh height and trims against the
    /// correct floor. Closes #1126.
    #[instrument(name = "ChainHeadService::tick", skip(self))]
    async fn tick(&self) -> Result<(), ChainHeadAdvanceError> {
        let tip = self.chain_tip().await?;
        metrics::gauge!(crate::metric_names::CHAIN_TIP_HEIGHT)
            .set(f64::from(u32::from(tip.height)));
        let previous = self.current.load_full();

        // Nothing to do when the source's tip is the one we hold. A block hash
        // commits to its parent, so an identical tip means an identical chain
        // beneath it — there is no reorg hiding below a tip we agree on.
        //
        // This is what keeps a steady-state poll to a single question. Without
        // it every tick rebuilds the graph and re-reads the tip block to check
        // for a same-height reorg, which costs a round trip per poll for an
        // answer that cannot have changed.
        if tip == previous.best_tip() {
            if !self.seeded.load(std::sync::atomic::Ordering::Acquire) {
                let next = self.seed_side_tips((*previous).clone()).await?;
                self.mark_fresh();
                self.publish_snapshot(&previous, next);
                return Ok(());
            }
            // the store moved under an unchanged head: the extensions catch up
            #[cfg(any(
                feature = "transactions",
                feature = "tree_state",
                feature = "history_tree"
            ))]
            if let Some(enabled) = &self.extensions {
                if crate::extension::store_moved(enabled, previous.extensions()) {
                    let mut next = (*previous).clone();
                    let slots =
                        crate::extension::advance(enabled, previous.extensions(), &next).await;
                    next.set_extensions(slots);
                    self.mark_fresh();
                    self.publish_snapshot(&previous, next);
                    return Ok(());
                }
            }
            self.mark_fresh();
            return Ok(());
        }

        let (next, disagreement) = self.next_graph(&previous, tip.height).await?;
        let next = self.seed_side_tips(next).await?;
        #[cfg(any(
            feature = "transactions",
            feature = "tree_state",
            feature = "history_tree"
        ))]
        let (previous, next, disagreement) = match &self.extensions {
            Some(enabled) => {
                let first = !self.folding.swap(true, std::sync::atomic::Ordering::AcqRel);
                let (previous, mut next, disagreement) = if first {
                    // the blocks are served, and `Ready`, before the first
                    // fold: starting up never waits on the extensions
                    self.mark(disagreement);
                    self.publish_snapshot(&previous, next);
                    let published = self.current.load_full();
                    let next = (*published).clone();
                    (published, next, None)
                } else {
                    (previous, next, disagreement)
                };
                let slots = crate::extension::advance(enabled, previous.extensions(), &next).await;
                next.set_extensions(slots);
                if first {
                    self.publish_snapshot(&previous, next);
                    return Ok(());
                }
                (previous, next, disagreement)
            }
            None => (previous, next, disagreement),
        };
        self.mark(disagreement);
        self.publish_snapshot(&previous, next);
        Ok(())
    }

    /// The status of an advance: fresh, or diverged from the validator's tip.
    fn mark(&self, disagreement: Option<TipDisagreement>) {
        match disagreement {
            Some(disagreement) => self.mark_diverged(disagreement),
            None => self.mark_fresh(),
        }
    }

    /// Publishes the [`TickOutcome::Advanced`] transition before the snapshot
    /// swap, so no reader can observe a fresh snapshot under a stale `Syncing`.
    fn mark_fresh(&self) {
        record(&self.status, TickOutcome::Advanced);
    }

    /// Advanced onto a branch heavier than the validator's tip: `Ready` but
    /// `Recoverable`, naming both tips, and counted.
    fn mark_diverged(&self, disagreement: TipDisagreement) {
        metrics::counter!(crate::metric_names::CHAIN_HEAD_TIP_DISAGREEMENTS_TOTAL).increment(1);
        self.status.degraded_unless_closing(
            Lifecycle::Ready,
            format!(
                "the validator's tip {} at {} is lighter than the retained branch at {} \
                 ({}); following the heavier",
                disagreement.validator.hash,
                disagreement.validator.height,
                disagreement.heavier.height,
                disagreement.heavier.hash,
            ),
        );
    }

    /// Builds the graph as it should be at `chain_height`.
    ///
    /// Returns a value; it neither reads nor writes the published cell. The old
    /// code mutated the published snapshot's clone in place and swapped it from
    /// inside this path, which is what let a long catch-up publish
    /// intermediates.
    async fn next_graph(
        &self,
        previous: &PersistentSnapshot,
        chain_height: Height,
    ) -> Result<(PersistentSnapshot, Option<TipDisagreement>), ChainHeadAdvanceError> {
        // Anchor floor: the chain head must never start more than the
        // configured depth below the chain tip. Previously this took the
        // greater of the finalised database's height and this floor; with the
        // database gone the floor is the whole rule, and it is the arm the old
        // code took whenever the database lagged (#1261).
        let anchor_height = height_below(chain_height, self.config.max_depth());

        let mut graph = if previous.best_tip().height < anchor_height {
            // The chain moved further than the window covers. Re-anchor rather
            // than walking the gap one block at a time.
            PersistentSnapshot::from_initial_block(
                anchor_block(self.source.as_ref(), anchor_height).await?,
            )
        } else {
            previous.clone()
        };

        // currently this only gets main-chain blocks
        // once readstateservice supports serving sidechain data, this
        // must be rewritten to match
        //
        // see https://github.com/ZcashFoundation/zebra/issues/9541
        //
        // Reads run `fetch_concurrency` heights ahead of the link and arrive
        // in order; each height's block and roots are independent reads. The
        // link stays sequential, and a reorg drops everything in flight: those
        // reads were addressed by height on a chain that just changed.
        'build: while u32::from(graph.best_tip().height) < u32::from(chain_height) {
            let first = next_height(graph.best_tip().height);
            let heights = std::iter::successors(Some(first), |height| Some(next_height(*height)))
                .take_while(|height| *height <= chain_height);
            let mut fetched = futures::stream::iter(heights)
                .map(|height| async move {
                    tokio::join!(
                        self.block_at_height(height),
                        self.source.get_commitment_tree_roots_by_height(height),
                    )
                })
                .buffered(self.config.fetch_concurrency().get());

            while let Some((block, roots)) = fetched.next().await {
                let Some(block) = block? else {
                    // Past the tip; the roots result is dropped unexamined, since
                    // its `HeightNotFound` there is expected, not a failure.
                    break 'build;
                };

                let parent_hash = block.header.prev_hash;
                if parent_hash == graph.best_tip().hash {
                    // Normal chain progression
                    let roots = match roots {
                        Ok((roots_hash, roots)) if roots_hash == block.header.hash => roots,
                        // A reorg swapped the best-chain block between the two
                        // reads, or the height-addressed read failed; the hash we
                        // hold is authoritative, so refetch by it.
                        _ => tree_roots(self.source.as_ref(), block.header.hash).await?,
                    };
                    let chainblock = chain_head_block(
                        block,
                        roots,
                        ParentWork::Retained(graph.tip_block().work),
                    )?;
                    info!(
                        height = u32::from(chainblock.height()),
                        hash = %chainblock.hash(),
                        "Syncing block"
                    );
                    extend(&mut graph, chainblock)?;
                } else {
                    // There's been a reorg. The fresh block is the new chaintip; we
                    // walk back from it to the fork, rewind the trunk there (the
                    // displaced blocks become a branch), then fetch afresh.
                    self.handle_reorg(&mut graph, &block).await?;
                    continue 'build;
                }
            }
        }

        self.check_for_nonhigher_reorgs(&mut graph).await?;

        // Trim to a fixed window below the tip. This was the greater of the
        // finalised database's height and this tip-relative cap; the cap is now
        // the whole rule, and it is what bounded memory before whenever the
        // database under-reported or was pinned at zero in ephemeral mode.
        // Trimming happens in `publish_snapshot`, after the handoff has read
        // the blocks that crossed the seam this iteration. Trimming first would
        // remove them before they could be handed off.

        // The heaviest retained chain is the best chain; on equal work the
        // validator's tip stays (the trunk wins ties).
        let validator_tip = graph.best_tip();
        let heaviest = graph.heaviest_block().clone();
        if heaviest.hash() == validator_tip.hash {
            return Ok((graph, None));
        }
        warn!(
            validator = ?validator_tip,
            heavier = ?heaviest.reference,
            "a retained branch outweighs the validator's tip; following the heavier"
        );
        self.handle_reorg(&mut graph, &heaviest).await?;
        Ok((
            graph,
            Some(TipDisagreement {
                validator: validator_tip,
                heavier: heaviest.reference,
            }),
        ))
    }

    /// Once, after the first advance: lays down each fully validated
    /// side-chain tip the validator lists that forks inside the window, as a
    /// retained branch. A validator that does not list tips (zebrad) is not
    /// asked again; a transport failure retries with the next advance.
    async fn seed_side_tips(
        &self,
        mut graph: PersistentSnapshot,
    ) -> Result<PersistentSnapshot, ChainHeadAdvanceError> {
        use std::sync::atomic::Ordering;
        use zaino_primitives::types::rpc::ChainTipStatus;

        if self.seeded.load(Ordering::Acquire) {
            return Ok(graph);
        }
        let tips = match self.source.get_chain_tips().await {
            Ok(tips) => tips,
            Err(zaino_source::QueryError::Domain(zaino_source::GetChainTipsError::Unsupported)) => {
                self.seeded.store(true, Ordering::Release);
                return Ok(graph);
            }
            Err(error) => return Err(advance_error(error, "chain_tips")),
        };
        let floor = u32::from(graph.lowest_retained_height());
        let best = graph
            .block_by_hash(&graph.best_tip().hash)
            .cloned()
            .ok_or_else(|| {
                ChainHeadAdvanceError::ReorgFailure("the best tip is not retained".to_string())
            })?;
        for tip in tips {
            let forks_in_window = u32::from(tip.height)
                .checked_sub(tip.branch_len)
                .is_some_and(|fork| fork >= floor);
            if tip.status != ChainTipStatus::ValidFork
                || !forks_in_window
                || graph.block_by_hash(&tip.hash).is_some()
            {
                continue;
            }
            let block = self.block_at_hash(tip.hash).await?.ok_or_else(|| {
                ChainHeadAdvanceError::InconsistentSource(format!(
                    "validator lists the tip {} but does not serve it",
                    tip.hash
                ))
            })?;
            // onto the side branch, then back: it stays as a retained branch
            self.handle_reorg(&mut graph, &block).await?;
            self.handle_reorg(&mut graph, &best).await?;
        }
        self.seeded.store(true, Ordering::Release);
        Ok(graph)
    }

    /// Handle a blockchain reorg by finding the common ancestor.
    ///
    /// Walks down from `block` to the first ancestor on the best chain,
    /// collecting the branch on the way, then lays that branch back down from
    /// the fork point up. The descent holds its pending blocks in a `Vec`
    /// rather than in call frames, so its depth costs heap and not stack.
    async fn handle_reorg(
        &self,
        graph: &mut PersistentSnapshot,
        block: &impl Block,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let mut branch: Vec<BranchBlock> = Vec::new();
        let mut parent_hash = block.parent_hash();
        let mut descended: u32 = 0;

        // Down to the fork point. A branch longer than the retained window
        // cannot rejoin the best chain within it, so the walk stops rather than
        // asking the validator for ancestors all the way to genesis.
        let fork_point = loop {
            if descended > self.max_retained_depth() {
                return Err(ChainHeadAdvanceError::ReorgFailure(
                    "reorg handling walked beyond the retained window".to_string(),
                ));
            }
            descended = descended.saturating_add(1);

            match graph.block_by_hash(&parent_hash).cloned() {
                // The parent is the fork point exactly when it is canonical, so
                // the rewind is also the test. A refused rewind leaves the
                // graph as it was, so the walk can carry on below it.
                Some(prev_block) => match graph.rewind_to(prev_block.reference) {
                    Ok(()) => break prev_block,
                    Err(NotOnBestChain) => {
                        parent_hash = prev_block.parent_hash;
                        branch.push(BranchBlock::Retained(Box::new(prev_block)));
                    }
                },
                None => {
                    let fetched = self.block_at_hash(parent_hash).await?.ok_or(
                        ChainHeadAdvanceError::InconsistentSource(format!(
                            "validator is missing block {parent_hash}, the parent of one it served"
                        )),
                    )?;
                    parent_hash = fetched.header.prev_hash;
                    branch.push(BranchBlock::Fetched(Box::new(fetched)));
                }
            }
        };

        // Back up from the fork point, oldest first, ending on the block that
        // started the walk. Each block's work accumulates from the one below,
        // so they go on in order.
        let mut prev_block = fork_point;
        for pending in branch.into_iter().rev() {
            prev_block = pending.to_chain_head_block(&prev_block, self).await?;
            extend(graph, prev_block.clone())?;
        }
        let chainblock = block.to_chain_head_block(&prev_block, self).await?;
        extend(graph, chainblock.clone())?;
        Ok(chainblock)
    }

    /// Catches a reorg that did not raise the tip.
    ///
    /// The extension loop only notices a reorg when it finds a *higher* block
    /// whose parent it does not hold. A branch swap at the same height, or a
    /// rollback, produces no such block — this is what sees those.
    async fn check_for_nonhigher_reorgs(
        &self,
        graph: &mut PersistentSnapshot,
    ) -> Result<(), ChainHeadAdvanceError> {
        let tip = graph.best_tip();
        let mut target_height = tip.height;

        loop {
            if let Some(block) = self.block_at_height(target_height).await? {
                if block.header.hash != tip.hash {
                    self.handle_reorg(graph, &block).await?;
                }
                return Ok(());
            }

            // The source cannot serve this height. Step down until it can,
            // bounded by the retained window: below that there is nothing left
            // in the graph for a reorg to rejoin.
            if u32::from(target_height) == 0 {
                return Ok(());
            }
            target_height = height_below(target_height, 1);
            if u32::from(target_height).saturating_add(self.max_retained_depth())
                < u32::from(tip.height)
            {
                return Err(ChainHeadAdvanceError::ReorgFailure(
                    "reorg detection stepped below the retained window".to_string(),
                ));
            }
        }
    }

    /// Installs a snapshot and tells everyone what changed.
    ///
    /// A plain `store`: the writer task is the only writer, so there is nothing
    /// to lose a compare-and-swap race against. One store per iteration is what
    /// makes a published snapshot always whole.
    fn publish_snapshot(&self, previous: &PersistentSnapshot, mut next: PersistentSnapshot) {
        let (stale_tip, new_tip) = (previous.best_tip(), next.best_tip());
        let tip_changed = new_tip != stale_tip;

        // Read from the snapshot being installed: the fork point is the first
        // ancestor of the old tip that the *new* chain still calls canonical.
        // Before the trim below: the lookup needs the old tip still in the graph
        let fork = tip_changed
            .then(|| next.find_fork_point(&stale_tip.hash))
            .flatten();
        // Read beside the fork point, so both describe the same pre-trim graph
        let retained_floor = next.lowest_retained_height();

        // Trim after the fork point and floor are read: both describe the
        // pre-trim graph.
        next.remove_finalized_blocks(height_below(
            next.best_tip().height,
            self.max_retained_depth(),
        ));

        // Stamped *before* the store, so a reader that captures the view and
        // asks for its epoch is told the epoch this publication carries rather
        // than whatever has been published since. The rule for which generation
        // that is belongs to the snapshot; this only supplies the two facts it
        // cannot know — what came before, and how far the epoch has ever got.
        //
        // The highest published generation is read into a local first because
        // `borrow()` holds a read guard for the whole enclosing statement and
        // the `send_replace` below wants the write lock, so inlining the read
        // deadlocks the channel against itself.
        let highest_published = self.updates.borrow().generation;
        next.stamp_generation(previous, highest_published);
        let generation = next.epoch().generation;

        self.current.store(Arc::new(next));

        if tip_changed {
            log_tip_change(stale_tip, new_tip);
            record_reorg(stale_tip, fork, retained_floor);

            self.updates.send_replace(ChainStateEpoch {
                generation,
                best_tip: new_tip,
            });
        }
    }

    /// How far below the tip blocks are retained.
    fn max_retained_depth(&self) -> u32 {
        self.config.max_depth().saturating_add(RETENTION_MARGIN)
    }

    /// One coherent height/hash pair from the source.
    async fn chain_tip(&self) -> Result<BlockRef, ChainHeadAdvanceError> {
        let (hash, height) = self
            .source
            .get_chain_tip()
            .await
            .map_err(|error| advance_error(error, "chain tip"))?;
        Ok(BlockRef { hash, height })
    }

    /// A best-chain block by height. `None` when the source has no such block.
    async fn block_at_height(
        &self,
        height: Height,
    ) -> Result<Option<zaino_primitives::types::Block>, ChainHeadAdvanceError> {
        present_or_absent(
            self.source.get_block(height).await,
            "block_at_height",
            |domain| match domain {
                zaino_source::GetBlockError::HeightNotFound(_) => true,
            },
        )
    }

    /// A block by hash, side-chain blocks included.
    async fn block_at_hash(
        &self,
        hash: BlockHash,
    ) -> Result<Option<zaino_primitives::types::Block>, ChainHeadAdvanceError> {
        present_or_absent(
            self.source.get_block_by_hash(hash).await,
            "block_at_hash",
            |domain| match domain {
                zaino_source::GetBlockByHashError::NotFound(_) => true,
            },
        )
    }
}

/// A block read as the chain head sees it: present, absent, or failed.
///
/// `is_absent` classifies the domain error, and is an exhaustive match at each
/// call site, so a future domain variant breaks the build there — the one place
/// that must decide whether it means end-of-chain. Absence is not a failure: the
/// extension loop reads past the tip by design, which is how it learns where the
/// tip is. Any other domain answer, and every transport failure, goes through
/// [`advance_error`].
fn present_or_absent<T, E, N>(
    read: Result<T, zaino_source::QueryError<E, N>>,
    context: &str,
    is_absent: impl FnOnce(&E) -> bool,
) -> Result<Option<T>, ChainHeadAdvanceError>
where
    E: fmt::Debug + fmt::Display,
    N: std::error::Error + Into<zaino_source::NonDomainError>,
{
    match read {
        Ok(block) => Ok(Some(block)),
        Err(zaino_source::QueryError::Domain(domain)) if is_absent(&domain) => {
            debug!(context, %domain, "source reports no block; treating as absent");
            Ok(None)
        }
        Err(error) => Err(advance_error(error, context)),
    }
}

impl<S: ChainHeadBlockSource> StatusSource for ChainHeadService<S> {
    fn status(&self) -> ComponentStatus {
        self.status.status()
    }
}

impl<S: ChainHeadBlockSource> Drop for ChainHeadService<S> {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self
            .task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            handle.abort();
        }
    }
}

/// What one writer iteration concluded about the published snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TickOutcome {
    /// The snapshot now matches the validator tip read this iteration.
    Advanced,
    /// The advance failed and the backoff ladder will retry it.
    Retrying,
    /// The advance failed `max_consecutive_failures` times and the writer is exiting.
    GaveUp,
}

/// The chain head's status transition rule, stated once: each outcome's
/// status, except that closing absorbs every outcome so a shutdown racing the
/// final iteration stays observable on every handle.
fn record(status: &StatusCell, outcome: TickOutcome) {
    let (lifecycle, health) = match outcome {
        TickOutcome::Advanced => (Lifecycle::Ready, Health::Healthy),
        TickOutcome::Retrying => (Lifecycle::Syncing, Health::Recoverable),
        TickOutcome::GaveUp => (Lifecycle::Offline, Health::Critical),
    };
    status.set_unless_closing(lifecycle, health);
}

/// Classifies a source [`QueryError`](zaino_source::QueryError) into a
/// [`ChainHeadAdvanceError`], the single home of the transport-vs-domain split.
///
/// A transport failure is threaded through unchanged as the `#[source]` of
/// [`SourceUnavailable`](ChainHeadAdvanceError::SourceUnavailable), so
/// `Error::source()` yields the underlying [`NonDomainError`](zaino_source::NonDomainError)
/// and its machine-readable failure mode. A domain rejection wraps no external
/// error, so it stays message-only under
/// [`InconsistentSource`](ChainHeadAdvanceError::InconsistentSource), tagged
/// with `context` to name the query that was refused.
fn advance_error<E, N>(
    error: zaino_source::QueryError<E, N>,
    context: &str,
) -> ChainHeadAdvanceError
where
    E: fmt::Debug + fmt::Display,
    N: std::error::Error + Into<zaino_source::NonDomainError>,
{
    match error {
        zaino_source::QueryError::NonDomain(non_domain) => {
            ChainHeadAdvanceError::SourceUnavailable(non_domain.into())
        }
        zaino_source::QueryError::Domain(domain) => {
            ChainHeadAdvanceError::InconsistentSource(format!("{context}: {domain}"))
        }
    }
}

/// A [`ChainHeadService`] before it is spawned: what it syncs from, how, and
/// which extensions it folds.
pub struct ChainHeadBuilder<S: ChainHeadBlockSource> {
    source: Arc<S>,
    config: ChainHeadConfig,
    cancel: CancellationToken,
    #[cfg(any(
        feature = "transactions",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    extensions: Option<crate::extension::Enabled>,
}

impl<S: ChainHeadBlockSource> ChainHeadBuilder<S> {
    /// Anchors the graph, then starts the writer task. See
    /// [`ChainHeadService::spawn`] for the shutdown contract.
    pub async fn spawn(self) -> Result<Arc<ChainHeadService<S>>, ChainHeadInitError> {
        ChainHeadService::started(self).await
    }

    /// An anchored service with no writer task, for tests that step it.
    #[cfg(any(test, feature = "testing"))]
    pub async fn spawn_without_writer(
        self,
    ) -> Result<Arc<ChainHeadService<S>>, ChainHeadInitError> {
        ChainHeadService::anchored(self).await
    }

    /// Enables the extensions (D-50): folds of `zaino-indexes` over the
    /// window, rooted on the store's tip (`None` = no store: the window's
    /// floor), under the validator's upgrade `schedule`.
    #[cfg(any(
        feature = "transactions",
        feature = "tree_state",
        feature = "history_tree"
    ))]
    pub fn extensions(
        self,
        schedule: Arc<[zaino_primitives::types::NetworkUpgradeInfo]>,
        store_tip: Option<Arc<dyn zaino_chain_head::StoreTip>>,
    ) -> ExtensionBuilder<S> {
        let folds = zaino_component::CpuBudget::detect()
            .concurrency(zaino_component::Share::ChainHead)
            .get();
        ExtensionBuilder {
            inner: self,
            enabled: crate::extension::Enabled {
                schedule,
                store_tip,
                folds: Arc::new(tokio::sync::Semaphore::new(folds)),
                #[cfg(feature = "transactions")]
                transactions: false,
                #[cfg(feature = "txout_set")]
                txout_set: None,
                #[cfg(feature = "address_history")]
                address_history: None,
                #[cfg(feature = "compact_blocks")]
                compact_blocks: None,
                #[cfg(feature = "tree_state")]
                tree_state: None,
                #[cfg(feature = "history_tree")]
                history_tree: None,
            },
        }
    }
}

/// A [`ChainHeadBuilder`] with extensions: each `with_*` enables one and
/// takes the ports it drives below the root.
#[cfg(any(
    feature = "transactions",
    feature = "tree_state",
    feature = "history_tree"
))]
pub struct ExtensionBuilder<S: ChainHeadBlockSource> {
    inner: ChainHeadBuilder<S>,
    enabled: crate::extension::Enabled,
}

#[cfg(any(
    feature = "transactions",
    feature = "tree_state",
    feature = "history_tree"
))]
impl<S: ChainHeadBlockSource> ExtensionBuilder<S> {
    /// Transaction positions and outpoint spenders inside the window (I-03,
    /// I-09). Needs nothing below the root.
    #[cfg(feature = "transactions")]
    pub fn with_transactions(mut self) -> Self {
        self.enabled.transactions = true;
        self
    }

    /// The UTXO set at the tip (I-11), seeded from `ports` at the root; the
    /// window's spends of outputs below the root are resolved through them.
    /// Enables `transactions`, which resolves the spends inside the window.
    #[cfg(feature = "txout_set")]
    pub fn with_txout_set<P>(mut self, ports: Arc<P>) -> Self
    where
        P: zaino_chain_head::PrevoutSource
            + zaino_chain_head::RemainingOutputs
            + zaino_chain_head::TxOutSetSeed,
    {
        self.enabled.transactions = true;
        self.enabled.txout_set = Some(crate::extension::txout_set::Ports {
            prevouts: crate::extension::below::prevouts(&ports),
            remaining: crate::extension::below::remaining(&ports),
            accumulator: crate::extension::below::accumulator(&ports),
        });
        self
    }

    /// Transparent address effects inside the window (I-10); the spends of
    /// outputs below the root are resolved through `prevouts`. Enables
    /// `transactions`.
    #[cfg(feature = "address_history")]
    pub fn with_address_history<P: zaino_chain_head::PrevoutSource>(
        mut self,
        prevouts: Arc<P>,
    ) -> Self {
        self.enabled.transactions = true;
        self.enabled.address_history = Some(crate::extension::below::prevouts(&prevouts));
        self
    }

    /// The window's compact blocks (I-05), fees included; the spends of
    /// outputs below the root are resolved through `prevouts`. Enables
    /// `transactions`.
    #[cfg(feature = "compact_blocks")]
    pub fn with_compact_blocks<P: zaino_chain_head::PrevoutSource>(
        mut self,
        prevouts: Arc<P>,
    ) -> Self {
        self.enabled.transactions = true;
        self.enabled.compact_blocks = Some(crate::extension::below::prevouts(&prevouts));
        self
    }

    /// The note commitment trees after each block (C2), seeded from `seed`
    /// at the root; the validator reports each block's frontiers and
    /// completed subtrees, by hash.
    #[cfg(feature = "tree_state")]
    pub fn with_tree_state<P: zaino_chain_head::TreeStateSeed>(mut self, seed: Arc<P>) -> Self
    where
        S: zaino_source::OneShotGetTreestateByHash + zaino_source::OneShotGetSubtreeRoots,
    {
        let source: Arc<S> = Arc::clone(&self.inner.source);
        let trees: Arc<dyn crate::extension::tree_state::TreeInputs> = source;
        self.enabled.tree_state = Some(crate::extension::tree_state::Ports { seed, trees });
        self
    }

    /// The history tree after each block (C6), seeded from `seed` at the root.
    #[cfg(feature = "history_tree")]
    pub fn with_history_tree<P: zaino_chain_head::HistoryTreeSeed>(mut self, seed: Arc<P>) -> Self {
        self.enabled.history_tree = Some(seed);
        self
    }

    /// Anchors the graph, then starts the writer task.
    pub async fn spawn(self) -> Result<Arc<ChainHeadService<S>>, ChainHeadInitError> {
        let mut inner = self.inner;
        inner.extensions = Some(self.enabled);
        inner.spawn().await
    }

    /// An anchored service with no writer task, for tests that step it.
    #[cfg(any(test, feature = "testing"))]
    pub async fn spawn_without_writer(
        self,
    ) -> Result<Arc<ChainHeadService<S>>, ChainHeadInitError> {
        let mut inner = self.inner;
        inner.extensions = Some(self.enabled);
        inner.spawn_without_writer().await
    }
}

/// The commitment tree roots after the block `hash`, from the source.
async fn tree_roots<S: ChainHeadBlockSource>(
    source: &S,
    hash: BlockHash,
) -> Result<TreeRoots, ChainHeadAdvanceError> {
    source
        .get_commitment_tree_roots(hash)
        .await
        .map_err(|error| advance_error(error, &format!("tree roots for block {hash}")))
}

/// The block at `height` built as the window's anchor, which sits below the reorg-possible range and seeds the fold with `RelativeChainWork::ZERO`.
async fn anchor_block<S: ChainHeadBlockSource>(
    source: &S,
    height: Height,
) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
    let block = source
        .get_block(height)
        .await
        .map_err(|error| advance_error(error, &format!("anchor block {height}")))?;
    let tree_roots = tree_roots(source, block.header.hash).await?;
    chain_head_block(block, tree_roots, ParentWork::Anchor)
}

/// What a block being built folds its own work onto.
#[derive(Clone, Copy, Debug)]
enum ParentWork {
    /// The block is the window's anchor, where work is measured from, so it folds onto nothing.
    Anchor,
    /// The block extends a retained parent that has accumulated this much work.
    Retained(RelativeChainWork),
}

/// Builds a [`ChainHeadBlock`] whose work is [`RelativeChainWork::ZERO`] for [`ParentWork::Anchor`] and otherwise its parent's total plus its own.
fn chain_head_block(
    block: zaino_primitives::types::Block,
    tree_roots: TreeRoots,
    parent: ParentWork,
) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
    let work = match parent {
        ParentWork::Retained(parent_work) => parent_work
            .accumulate(block.header.bits.to_work())
            .map_err(|error| {
                ChainHeadAdvanceError::InconsistentSource(format!(
                    "work overflowed at block {}: {error}",
                    block.header.hash
                ))
            })?,
        ParentWork::Anchor => RelativeChainWork::ZERO,
    };

    Ok(ChainHeadBlock {
        reference: BlockRef {
            hash: block.header.hash,
            height: block.header.height,
        },
        parent_hash: block.header.prev_hash,
        work,
        block,
        tree_roots,
    })
}

/// A thin wrapper over [`ChainGraph::extend`] that does nothing but map its refusal to [`ChainHeadAdvanceError::InconsistentSource`], because a block the source served that does not attach where it was asked for is inconsistent source data.
///
/// [`ChainGraph::extend`]: crate::graph::ChainGraph::extend
fn extend(
    graph: &mut PersistentSnapshot,
    block: ChainHeadBlock,
) -> Result<(), ChainHeadAdvanceError> {
    graph.extend(block).map_err(|refused: NotChildOfTip| {
        ChainHeadAdvanceError::InconsistentSource(refused.to_string())
    })
}

/// Anchors the graph, retrying transient source failures.
async fn anchor_with_retry<S: ChainHeadBlockSource>(
    source: &Arc<S>,
    config: &ChainHeadConfig,
    cancel: &CancellationToken,
) -> Result<PersistentSnapshot, ChainHeadInitError> {
    let mut backoff = config.initial_backoff();
    let mut failures = 0u32;

    loop {
        if cancel.is_cancelled() {
            return Err(ChainHeadInitError::Cancelled);
        }

        match anchor(source, config).await {
            Ok(snapshot) => return Ok(snapshot),
            Err(error) => {
                failures = failures.saturating_add(1);
                if failures >= config.max_consecutive_failures() {
                    return Err(ChainHeadInitError::SourceUnavailable {
                        attempts: failures,
                        source: error,
                    });
                }
                warn!(%error, attempt = failures, "ChainHead anchoring failed; retrying");
                if sleep_or_cancel(backoff, cancel).await.is_break() {
                    return Err(ChainHeadInitError::Cancelled);
                }
                backoff = backoff.saturating_mul(2).min(config.max_backoff());
            }
        }
    }
}

/// One attempt at anchoring: the block at `tip - depth`, alone.
///
/// The writer task extends from here one block at a time, exactly as before.
async fn anchor<S: ChainHeadBlockSource>(
    source: &Arc<S>,
    config: &ChainHeadConfig,
) -> Result<PersistentSnapshot, ChainHeadAdvanceError> {
    let (_, tip_height) = source
        .get_chain_tip()
        .await
        .map_err(|error| advance_error(error, "chain tip"))?;

    let anchor_height = height_below(tip_height, config.max_depth());

    Ok(PersistentSnapshot::from_initial_block(
        anchor_block(source.as_ref(), anchor_height).await?,
    ))
}

/// Sleeps, unless cancelled first.
async fn sleep_or_cancel(
    duration: Duration,
    cancel: &CancellationToken,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        _ = cancel.cancelled() => std::ops::ControlFlow::Break(()),
        _ = tokio::time::sleep(duration) => std::ops::ControlFlow::Continue(()),
    }
}

/// `height - delta`, saturating at genesis.
fn height_below(height: Height, delta: u32) -> Height {
    height.saturating_sub(delta)
}

/// The height one above `height`, saturating at the protocol maximum.
fn next_height(height: Height) -> Height {
    height.checked_add(1).unwrap_or(height)
}

fn log_tip_change(old: BlockRef, new: BlockRef) {
    let (old_height, new_height) = (u32::from(old.height), u32::from(new.height));
    if new_height > old_height {
        info!(old_height, new_height, new_hash = %new.hash, "Chain head tip advanced");
    } else if new_height == old_height {
        info!(height = new_height, old_hash = %old.hash, new_hash = %new.hash, "Chain head tip reorg");
    } else {
        info!(old_height, new_height, new_hash = %new.hash, "Chain head tip rollback");
    }
}

/// How a tip change relates to the tip it replaced
///
/// - `Reorg` depth = blocks rewritten; `None` = fork point below the retained window
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TipChange {
    Advance,
    Reorg(Option<u32>),
}

/// Classifies a tip change from the fork point the new snapshot reports.
///
/// - Reorg = the old tip stopped being canonical, whatever the heights. A branch won
///   by a *longer* chain is the ordinary case, and comparing heights misses it
/// - Depth = blocks rewritten (old tip - fork), not height lost: an equal-height swap
///   rewrites one block, and a longer branch rewrites what it replaced
/// - No fork point and the old tip below `retained_floor` = the chain advanced past
///   the window (or the graph re-anchored), so the old tip was dropped, not replaced;
///   a reorg that deep is beyond the consensus seam the window is sized to
pub(crate) fn classify_tip_change(
    old: BlockRef,
    fork: Option<BlockRef>,
    retained_floor: Height,
) -> TipChange {
    match fork {
        // The old tip is still canonical, so the new one descends from it
        Some(fork) if fork == old => TipChange::Advance,
        Some(fork) => TipChange::Reorg(Some(
            u32::from(old.height).saturating_sub(u32::from(fork.height)),
        )),
        None if old.height < retained_floor => TipChange::Advance,
        None => TipChange::Reorg(None),
    }
}

/// Sets the sync failure gauges; zero failures means healthy.
fn record_sync_failures(consecutive_failures: u32, backoff: Duration) {
    use crate::metric_names::{SYNC_BACKOFF_SECONDS, SYNC_CONSECUTIVE_FAILURES};

    metrics::gauge!(SYNC_CONSECUTIVE_FAILURES).set(f64::from(consecutive_failures));
    metrics::gauge!(SYNC_BACKOFF_SECONDS).set(backoff.as_secs_f64());
}

/// Reports a tip change that rewrote part of the chain.
fn record_reorg(old: BlockRef, fork: Option<BlockRef>, retained_floor: Height) {
    use crate::metric_names::{CHAIN_HEAD_REORG_DEPTH, CHAIN_HEAD_REORG_TOTAL};

    let depth = match classify_tip_change(old, fork, retained_floor) {
        TipChange::Advance => return,
        TipChange::Reorg(depth) => depth,
    };

    metrics::counter!(CHAIN_HEAD_REORG_TOTAL).increment(1);
    match depth {
        Some(depth) => metrics::histogram!(CHAIN_HEAD_REORG_DEPTH).record(f64::from(depth)),
        // Counted but not measured, so `reorg_total` can exceed the histogram's count
        None => warn!(
            old_tip = %old.hash,
            "reorg forked below the retained window; depth unknown"
        ),
    }
}

/// Lets the reorg walk take either a block already in the graph or one just
/// fetched from the source, as the original's private `Block` trait did.
///
/// The original compared serialized-order hash bytes because its two block
/// types disagreed about byte order. Both sides now carry the domain's
/// [`BlockHash`], so the comparison is direct.
trait Block {
    fn parent_hash(&self) -> BlockHash;
    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError>;
}

/// One block on a branch the reorg walk has yet to lay down.
///
/// The walk meets two kinds on its way to the fork point: blocks the graph
/// still holds on a branch that lost, and blocks it has to ask the validator
/// for. Both go on the same pending list, so the list has one type.
/// Both variants are boxed: a whole block runs to well over a kilobyte, and the
/// list holds one entry per block on the branch.
enum BranchBlock {
    /// Retained by the graph, on a branch that is not the best chain.
    Retained(Box<ChainHeadBlock>),
    /// Fetched from the validator, because the graph had dropped it.
    Fetched(Box<zaino_primitives::types::Block>),
}

impl Block for BranchBlock {
    fn parent_hash(&self) -> BlockHash {
        match self {
            Self::Retained(block) => block.parent_hash(),
            Self::Fetched(block) => block.parent_hash(),
        }
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        match self {
            Self::Retained(block) => block.to_chain_head_block(prev_block, service).await,
            Self::Fetched(block) => block.to_chain_head_block(prev_block, service).await,
        }
    }
}

impl Block for ChainHeadBlock {
    fn parent_hash(&self) -> BlockHash {
        self.parent_hash
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        _prev_block: &ChainHeadBlock,
        _service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        Ok(self.clone())
    }
}

impl Block for zaino_primitives::types::Block {
    fn parent_hash(&self) -> BlockHash {
        self.header.prev_hash
    }

    async fn to_chain_head_block<S: ChainHeadBlockSource>(
        &self,
        prev_block: &ChainHeadBlock,
        service: &ChainHeadService<S>,
    ) -> Result<ChainHeadBlock, ChainHeadAdvanceError> {
        let tree_roots = tree_roots(service.source.as_ref(), self.header.hash).await?;
        chain_head_block(
            self.clone(),
            tree_roots,
            ParentWork::Retained(prev_block.work),
        )
    }
}

#[cfg(test)]
mod next_status_rule {
    use super::{record, StatusCell, TickOutcome, COMPONENT};
    use zaino_component::{ComponentName, Health, Lifecycle, StatusSource as _};

    const OUTCOMES: [TickOutcome; 3] = [
        TickOutcome::Advanced,
        TickOutcome::Retrying,
        TickOutcome::GaveUp,
    ];

    /// No outcome may overwrite `Closing`, so a shutdown stays observable.
    #[test]
    fn closing_absorbs_every_outcome() {
        for outcome in OUTCOMES {
            let cell = StatusCell::new(
                ComponentName(COMPONENT),
                Lifecycle::Closing,
                Health::Healthy,
            );
            record(&cell, outcome);
            assert_eq!(cell.status().lifecycle, Lifecycle::Closing, "{outcome:?}");
        }
    }

    /// From any other phase, each outcome takes its own status.
    #[test]
    fn every_live_state_takes_the_outcome_status() {
        for lifecycle in [
            Lifecycle::Spawning,
            Lifecycle::Syncing,
            Lifecycle::Ready,
            Lifecycle::Offline,
        ] {
            for (outcome, expected) in [
                (TickOutcome::Advanced, (Lifecycle::Ready, Health::Healthy)),
                (
                    TickOutcome::Retrying,
                    (Lifecycle::Syncing, Health::Recoverable),
                ),
                (TickOutcome::GaveUp, (Lifecycle::Offline, Health::Critical)),
            ] {
                let cell = StatusCell::new(ComponentName(COMPONENT), lifecycle, Health::Healthy);
                record(&cell, outcome);
                let status = cell.status();
                assert_eq!((status.lifecycle, status.health), expected, "{outcome:?}");
            }
        }
    }
}

#[cfg(test)]
mod reorg_tests {
    use super::*;

    fn at(height: u32, hash: u8) -> BlockRef {
        BlockRef {
            hash: BlockHash::from([hash; 32]),
            height: Height::try_from(height).expect("height fits"),
        }
    }

    /// A retention floor below every old tip these tests use, so it decides nothing.
    fn floor() -> Height {
        at(90, 0).height
    }

    /// - Old tip still canonical → the new tip descends from it, however many blocks
    ///   arrived at once
    #[test]
    fn extending_the_chain_is_not_a_reorg() {
        let old = at(100, 1);
        assert_eq!(
            classify_tip_change(old, Some(old), floor()),
            TipChange::Advance
        );
    }

    /// - The case the old height comparison dropped entirely: a competing branch wins
    ///   *because* it is longer, so the new tip is higher and 3 blocks were still rewritten
    #[test]
    fn a_reorg_won_by_a_longer_chain_is_counted_with_its_true_depth() {
        assert_eq!(
            classify_tip_change(at(100, 1), Some(at(97, 9)), floor()),
            TipChange::Reorg(Some(3)),
            "100 -> fork at 97 rewrites 3 blocks, whatever height the new tip reached"
        );
    }

    /// - One block replaced by another at the same height rewrites one block, not zero
    #[test]
    fn an_equal_height_swap_rewrites_one_block() {
        assert_eq!(
            classify_tip_change(at(100, 1), Some(at(99, 9)), floor()),
            TipChange::Reorg(Some(1))
        );
    }

    /// - A rollback to a lower canonical tip is a reorg of the distance rolled back
    #[test]
    fn a_rollback_reports_the_distance_lost() {
        assert_eq!(
            classify_tip_change(at(100, 1), Some(at(95, 9)), floor()),
            TipChange::Reorg(Some(5))
        );
    }

    /// - Counted, not measured: the alternative is inventing a depth the window
    ///   cannot see
    #[test]
    fn a_fork_below_the_window_is_a_reorg_of_unknown_depth() {
        assert_eq!(
            classify_tip_change(at(100, 1), None, floor()),
            TipChange::Reorg(None)
        );
    }

    /// An old tip below the retained floor was dropped by the window, not replaced by a branch.
    #[test]
    fn an_old_tip_below_the_retained_floor_is_an_advance() {
        assert_eq!(
            classify_tip_change(at(100, 1), None, at(101, 0).height),
            TipChange::Advance
        );
    }

    /// An old tip at the floor is still retained, so its absence from the chain is a real reorg.
    #[test]
    fn an_old_tip_at_the_retained_floor_is_still_a_reorg() {
        assert_eq!(
            classify_tip_change(at(100, 1), None, at(100, 0).height),
            TipChange::Reorg(None)
        );
    }
}
