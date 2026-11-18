//! The extensions (D-50) over the shared fake validator. Below the root the
//! ports are answered by folding each index from genesis over the chain, so
//! every view and every answer is checked against a fold the extension did
//! not make.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use zaino_chain_head::{
    extension::{
        HistoryEpochSeed, HistoryTreeSeed, PrevoutSource, RemainingOutputs, SeedError, StoreTip,
        TreeStateSeed, TxOutSetSeed,
    },
    snapshot::{
        ChainHeadHistoryTreeService as _, ChainHeadTransactionService as _,
        ChainHeadTransparentHistoryService as _, ChainHeadTreeStateService as _,
        ChainHeadTxOutSetService as _,
    },
    transparent::TransparentHistoryQuery,
    ChainHeadBlockService as _, ChainHeadError, ChainHeadSnapshot as _, HeadCapability,
};
use zaino_indexes::{
    address_history, assemble, history_tree, spent, tree_state, tx_out_set, txid_location,
    CompletedSubtree, FoldBlock, FoldBlockParts, FoldTx, Index, PoolFrontiers,
};
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend as _, BackendWriter as _, Namespace};
use zaino_primitives::txout_commitment::{commitment_address, is_unspendable, TxOutSetAccumulator};
use zaino_primitives::types::{
    branch_at, BlockRef, ConsensusBranchId, Height, NetworkUpgradeInfo, Outpoint, PoolFrontier,
    ShieldedPool, TransactionId, TransparentAddressKey, TransparentOutput,
};
use zaino_source_mock::{next_block, Activations, BlockSpec, ChainBuilder, MockBlock, MockChain};

use super::test_config;
use crate::service::ChainHeadService;
use crate::snapshot::PersistentSnapshot;

/// Notes in two pools, matured coinbases, spends (chained inside a block),
/// then plain blocks.
fn chain() -> MockChain {
    ChainBuilder::new()
        .blocks(3)
        .block(BlockSpec::default().notes(ShieldedPool::Sapling, 5))
        .block(BlockSpec::default().notes(ShieldedPool::Orchard, 3))
        .matured()
        .blocks(3)
        .block(BlockSpec::spends(4).chained(2))
        .block(BlockSpec::spends(2))
        .blocks(2)
        .build()
}

fn schedule() -> Arc<[NetworkUpgradeInfo]> {
    Arc::from(Activations::default().upgrades())
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("small")
}

fn tip_height(chain: &MockChain) -> u32 {
    u32::from(chain.tip().expect("a chain").height())
}

fn reference(block: &MockBlock) -> BlockRef {
    BlockRef {
        hash: block.hash(),
        height: block.height(),
    }
}

/// The chain's blocks `0..=through`.
fn blocks_through(chain: &MockChain, through: u32) -> Vec<MockBlock> {
    (0..=through)
        .map(|h| chain.block_at(h).expect("held"))
        .collect()
}

/// `mock` as fold input, its prevouts from `outputs`, which it then extends.
fn fold_block_of(
    mock: &MockBlock,
    schedule: &[NetworkUpgradeInfo],
    outputs: &mut HashMap<Outpoint, TransparentOutput>,
) -> FoldBlock {
    // in transaction order: a transaction may spend an earlier one's output
    let mut transactions = Vec::new();
    for tx in mock.block.transactions() {
        let prevouts = tx
            .transparent
            .inputs
            .iter()
            .map(|input| {
                outputs[&Outpoint {
                    txid: input.prev_txid,
                    index: input.prev_index,
                }]
                    .clone()
            })
            .collect();
        transactions.push(FoldTx::from_transaction(tx, Some(prevouts)).expect("pairs"));
        for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
            outputs.insert(
                Outpoint {
                    txid: tx.txid,
                    index,
                },
                output.clone(),
            );
        }
    }
    let mut frontiers = PoolFrontiers::default();
    for pool in assemble::changed_pools(mock.block.transactions(), &mock.tree_roots) {
        let tree = match pool.pool {
            ShieldedPool::Sapling => mock.trees.sapling.as_ref(),
            ShieldedPool::Orchard => mock.trees.orchard.as_ref(),
            ShieldedPool::Ironwood => mock.trees.ironwood.as_ref(),
        };
        let frontier = assemble::frontier_of(tree, pool.pool).expect("a frontier");
        match pool.pool {
            ShieldedPool::Sapling => frontiers.sapling = Some(frontier),
            ShieldedPool::Orchard => frontiers.orchard = Some(frontier),
            ShieldedPool::Ironwood => frontiers.ironwood = Some(frontier),
        }
    }
    let subtrees = mock
        .subtrees
        .iter()
        .map(|(pool, index, root)| CompletedSubtree {
            pool: *pool,
            index: u64::from(*index),
            root: root.root.into(),
        })
        .collect();
    FoldBlock::try_new(FoldBlockParts {
        header: mock.block.header.clone(),
        branch: branch_at(schedule, mock.height()),
        transactions,
        tree_roots: mock.tree_roots.clone(),
        frontiers,
        subtrees,
    })
    .expect("a fold block")
}

/// The chain's blocks `0..=through` as fold input.
fn fold_blocks(chain: &MockChain, through: u32) -> Vec<FoldBlock> {
    fold_blocks_of(&blocks_through(chain, through))
}

/// `I` folded over `blocks` from `state`: the backend holding the rows written,
/// and the state after the last block.
fn fold_from<I: Index>(
    state: Option<I::State>,
    blocks: &[FoldBlock],
) -> (InMemoryBackend, Option<I::State>) {
    let backend = InMemoryBackend::new(&I::SCHEMA.specs()).expect("specs");
    let mut state = state;
    for block in blocks {
        let fold = I::fold(state.as_ref(), block).expect("folds");
        backend
            .writer()
            .expect("writer")
            .commit(block.reference(), fold.ops)
            .expect("commits");
        state = Some(fold.next);
    }
    (backend, state)
}

/// `I` folded over the chain through `through`.
fn folded<I: Index>(chain: &MockChain, through: u32) -> I::State {
    fold_from::<I>(None, &fold_blocks(chain, through))
        .1
        .expect("at least one block")
}

/// The ports below the root, answered from the chain itself; the store's tip
/// is set by the test.
struct Below {
    chain: MockChain,
    failing: AtomicBool,
    tip: Mutex<Option<BlockRef>>,
}

impl StoreTip for Below {
    fn store_tip(&self) -> Option<BlockRef> {
        *self.tip.lock().expect("tip lock")
    }
}

impl Below {
    fn set_tip(&self, tip: BlockRef) {
        *self.tip.lock().expect("tip lock") = Some(tip);
    }

    fn guard(&self, root: BlockRef) -> Result<(), SeedError> {
        if self.failing.load(Ordering::SeqCst) {
            return Err(SeedError::Unavailable {
                message: "injected".to_owned(),
                cause: None,
            });
        }
        match self.chain.block_at(u32::from(root.height)) {
            Some(block) if block.hash() == root.hash => Ok(()),
            _ => Err(SeedError::RootNotHeld { root }),
        }
    }
}

impl PrevoutSource for Below {
    async fn previous_outputs(
        &self,
        root: BlockRef,
        outpoints: &[Outpoint],
    ) -> Result<Vec<Option<TransparentOutput>>, SeedError> {
        self.guard(root)?;
        let mut outputs = HashMap::new();
        for mock in blocks_through(&self.chain, u32::from(root.height)) {
            for tx in mock.block.transactions() {
                for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                    outputs.insert(
                        Outpoint {
                            txid: tx.txid,
                            index,
                        },
                        output.clone(),
                    );
                }
            }
        }
        Ok(outpoints
            .iter()
            .map(|outpoint| outputs.get(outpoint).cloned())
            .collect())
    }
}

impl RemainingOutputs for Below {
    async fn remaining_outputs(
        &self,
        root: BlockRef,
        txids: &[TransactionId],
    ) -> Result<Vec<Option<u32>>, SeedError> {
        self.guard(root)?;
        let state = folded::<tx_out_set::TxOutSet>(&self.chain, u32::from(root.height));
        Ok(txids
            .iter()
            .map(|txid| state.remaining.get(txid).copied())
            .collect())
    }
}

impl TxOutSetSeed for Below {
    async fn accumulator_at(&self, root: BlockRef) -> Result<TxOutSetAccumulator, SeedError> {
        self.guard(root)?;
        Ok(folded::<tx_out_set::TxOutSet>(&self.chain, u32::from(root.height)).accumulator)
    }
}

fn frontier_of_tree(tree: &Option<tree_state::PoolTree>) -> PoolFrontier {
    match tree {
        None => PoolFrontier::Absent,
        Some(tree_state::PoolTree { frontier: None, .. }) => PoolFrontier::Empty,
        Some(tree_state::PoolTree {
            frontier: Some(frontier),
            ..
        }) => PoolFrontier::Frontier(frontier.clone()),
    }
}

impl TreeStateSeed for Below {
    async fn frontiers_at(&self, root: BlockRef) -> Result<[PoolFrontier; 3], SeedError> {
        self.guard(root)?;
        let state = folded::<tree_state::TreeState>(&self.chain, u32::from(root.height));
        let [sapling, orchard, ironwood] = &state.pools;
        Ok([
            frontier_of_tree(sapling),
            frontier_of_tree(orchard),
            frontier_of_tree(ironwood),
        ])
    }
}

impl HistoryTreeSeed for Below {
    async fn epoch_at(&self, root: BlockRef) -> Result<Option<HistoryEpochSeed>, SeedError> {
        self.guard(root)?;
        let state = folded::<history_tree::HistoryTree>(&self.chain, u32::from(root.height));
        Ok(state.epoch.map(|epoch| HistoryEpochSeed {
            branch: ConsensusBranchId::from(epoch.branch),
            length: epoch.length,
            peaks: epoch.peaks,
        }))
    }
}

/// A stepped head with every extension and its ports.
struct Harness {
    chain: MockChain,
    below: Arc<Below>,
    service: Arc<ChainHeadService<MockChain>>,
}

impl Harness {
    /// Anchored `max_depth` below the tip, the store's tip at `root_height`,
    /// advanced once to the tip.
    async fn new(chain: MockChain, max_depth: u32, root_height: u32) -> Self {
        let root_block = chain.block_at(root_height).expect("root held");
        let below = Arc::new(Below {
            chain: chain.clone(),
            failing: AtomicBool::new(false),
            tip: Mutex::new(Some(reference(&root_block))),
        });
        let service = ChainHeadService::builder(
            Arc::new(chain.clone()),
            test_config(max_depth),
            CancellationToken::new(),
        )
        .extensions(schedule(), Some(below.clone()))
        .with_transactions()
        .with_txout_set(Arc::clone(&below))
        .with_address_history(Arc::clone(&below))
        .with_tree_state(Arc::clone(&below))
        .with_history_tree(Arc::clone(&below))
        .spawn_without_writer()
        .await
        .expect("anchors");
        service.advance_once().await.expect("advances");
        Self {
            chain,
            below,
            service,
        }
    }

    fn snapshot(&self) -> Arc<PersistentSnapshot> {
        self.service.subscriber().current()
    }

    fn root(&self) -> BlockRef {
        self.below.store_tip().expect("rooted")
    }

    /// One block appended to the chain, then one tick.
    async fn extend_and_advance(&self, spec: BlockSpec) {
        let parent = self.chain.tip().expect("a chain");
        self.chain
            .extend(next_block(Some(&parent), &Activations::default(), spec));
        self.service.advance_once().await.expect("advances");
    }
}

/// Every map row of `namespace` as `reader` holds it.
fn rows(
    reader: &impl zaino_persistence::MaintenanceReader,
    namespace: Namespace,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    reader.scan(namespace).expect("scans")
}

/// The window's blocks above the root, trunk order.
fn window(
    snapshot: &PersistentSnapshot,
    root: BlockRef,
) -> Vec<Arc<zaino_chain_head::ChainHeadBlock>> {
    snapshot.above(root.height).cloned().collect()
}

/// The accumulator over the whole chain through `through`, computed without
/// any index.
fn independent_accumulator(chain: &MockChain, through: u32) -> TxOutSetAccumulator {
    let mut unspent: HashMap<Outpoint, TransparentOutput> = HashMap::new();
    for mock in blocks_through(chain, through).iter().skip(1) {
        for tx in mock.block.transactions() {
            for input in &tx.transparent.inputs {
                unspent.remove(&Outpoint {
                    txid: input.prev_txid,
                    index: input.prev_index,
                });
            }
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                if !is_unspendable(output.script.as_bytes()) {
                    unspent.insert(
                        Outpoint {
                            txid: tx.txid,
                            index,
                        },
                        output.clone(),
                    );
                }
            }
        }
    }
    let mut expected = TxOutSetAccumulator::empty();
    for (outpoint, output) in &unspent {
        expected
            .apply_added_output(
                outpoint,
                output.value,
                &commitment_address(output.script.as_bytes()),
            )
            .expect("adds");
    }
    let mut txids: Vec<TransactionId> = unspent.keys().map(|outpoint| outpoint.txid).collect();
    txids.sort_unstable_by_key(|txid| <[u8; 32]>::from(*txid));
    txids.dedup();
    expected.transactions = txids.len() as u64;
    expected
}

/// Every view above the root holds exactly what a fold from the root's state
/// over the trunk writes, and the fold states agree with a fold from genesis.
#[tokio::test]
async fn every_view_equals_a_fold_from_genesis() {
    let chain = chain();
    let tip = tip_height(&chain);
    let harness = Harness::new(chain.clone(), 20, tip - 18).await;
    assert_views_are_folds(
        &harness.snapshot(),
        harness.root(),
        &[blocks_through(&chain, tip)],
    );
}

/// After a reorg the displaced blocks stay retained on a branch: every view,
/// trunk and branch alike, equals a fold from genesis along its own ancestry.
#[tokio::test]
async fn every_branch_view_equals_a_fold_along_its_ancestry() {
    let chain = chain();
    let tip = tip_height(&chain);
    let harness = Harness::new(chain.clone(), 20, tip - 18).await;
    let displaced = blocks_through(&chain, tip);
    let activations = Activations::default();
    let mut parent = chain.block_at(tip - 4).expect("held");
    let mut branch = Vec::new();
    for seed in 1..=5u64 {
        let block = next_block(
            Some(&parent),
            &activations,
            BlockSpec::spends(1).seeded(seed),
        );
        parent = block.clone();
        branch.push(block);
    }
    chain.reorg(tip - 3, branch);
    harness.service.advance_once().await.expect("advances");
    let after = harness.snapshot();
    let root = harness.root();
    assert!(
        window(&after, root)
            .iter()
            .any(|block| !after.is_trunk_block(block.reference)),
        "the displaced blocks are retained on a branch"
    );
    assert_views_are_folds(&after, root, &[blocks_through(&chain, tip + 1), displaced]);
}

/// The chain's blocks as fold input, genesis first.
fn fold_blocks_of(mocks: &[MockBlock]) -> Vec<FoldBlock> {
    let schedule = schedule();
    let mut outputs = HashMap::new();
    mocks
        .iter()
        .map(|mock| fold_block_of(mock, &schedule, &mut outputs))
        .collect()
}

/// Every view above `root` holds exactly what a fold from the root's state
/// writes along its block's own ancestry, one of `chains` (each genesis
/// first), and its fold states agree with a fold from genesis.
fn assert_views_are_folds(
    snapshot: &PersistentSnapshot,
    root: BlockRef,
    chains: &[Vec<MockBlock>],
) {
    assert!(
        snapshot.extension_failures().is_empty(),
        "{:?}",
        snapshot.extension_failures()
    );
    let folded: Vec<(&Vec<MockBlock>, Vec<FoldBlock>)> = chains
        .iter()
        .map(|mocks| (mocks, fold_blocks_of(mocks)))
        .collect();
    let at = |h: u32| usize::try_from(h).expect("small");
    let slots = snapshot.extensions();
    let transactions = slots
        .transactions
        .as_ref()
        .and_then(|slot| slot.built())
        .expect("built");
    let txout_set = slots
        .txout_set
        .as_ref()
        .and_then(|slot| slot.built())
        .expect("built");
    let trees = slots
        .tree_state
        .as_ref()
        .and_then(|slot| slot.built())
        .expect("built");
    let history = slots
        .history_tree
        .as_ref()
        .and_then(|slot| slot.built())
        .expect("built");
    let addresses = slots
        .address_history
        .as_ref()
        .and_then(|slot| slot.built())
        .expect("built");

    for block in window(snapshot, root) {
        let h = u32::from(block.height());
        let (_, blocks) = folded
            .iter()
            .find(|(mocks, _)| {
                mocks
                    .get(at(h))
                    .is_some_and(|mock| mock.hash() == block.hash())
            })
            .expect("the block is on one of the chains");
        let through = &blocks[..=at(h)];
        let from_root = &blocks[at(u32::from(root.height)) + 1..=at(h)];

        // map-keyed indexes: the overlay's rows are exactly the window's
        let locations = transactions.locations.view(&block.hash()).expect("a view");
        let (expected, _) = fold_from::<txid_location::TxidLocation>(
            Some(txid_location::State { tip: root }),
            from_root,
        );
        for spec in txid_location::TxidLocation::SCHEMA.specs() {
            assert_eq!(
                rows(&locations.overlay, spec.namespace),
                rows(&expected.reader().expect("reader"), spec.namespace),
                "txid_location {} at {h}",
                spec.namespace
            );
        }
        let spenders = transactions.spent.view(&block.hash()).expect("a view");
        let (expected, _) = fold_from::<spent::Spent>(Some(spent::State { tip: root }), from_root);
        for spec in spent::Spent::SCHEMA.specs() {
            assert_eq!(
                rows(&spenders.overlay, spec.namespace),
                rows(&expected.reader().expect("reader"), spec.namespace),
                "spent {} at {h}",
                spec.namespace
            );
        }
        let events = addresses.view(&block.hash()).expect("a view");
        let (expected, _) = fold_from::<address_history::AddressHistory>(
            Some(address_history::State { tip: root }),
            from_root,
        );
        for spec in address_history::AddressHistory::SCHEMA.specs() {
            assert_eq!(
                rows(&events.overlay, spec.namespace),
                rows(&expected.reader().expect("reader"), spec.namespace),
                "address_history {} at {h}",
                spec.namespace
            );
        }

        // stateful indexes: the state after the block is the state a fold from
        // genesis reaches
        let (_, expected) = fold_from::<tx_out_set::TxOutSet>(None, through);
        let expected = expected.expect("folded");
        let set = txout_set.view(&block.hash()).expect("a view");
        assert_eq!(
            set.state.accumulator, expected.accumulator,
            "txout set at {h}"
        );
        let (_, expected) = fold_from::<tree_state::TreeState>(None, through);
        let tree = trees.view(&block.hash()).expect("a view");
        assert_eq!(
            tree.state.pools,
            expected.expect("folded").pools,
            "trees at {h}"
        );
        let (_, expected) = fold_from::<history_tree::HistoryTree>(None, through);
        let expected = expected.expect("folded").epoch.expect("an epoch");
        let epoch = history
            .view(&block.hash())
            .expect("a view")
            .state
            .epoch
            .clone()
            .expect("an epoch");
        assert_eq!(epoch.branch, expected.branch, "history branch at {h}");
        assert_eq!(epoch.length, expected.length, "history length at {h}");
        assert_eq!(epoch.peaks, expected.peaks, "history peaks at {h}");
        assert_eq!(
            history_tree::root_of(&epoch).expect("a root"),
            history_tree::root_of(&expected).expect("a root"),
            "history root at {h}"
        );
    }
}

/// A built extension that cannot read what it must hold answers `ReadFailed`,
/// never an empty or absent answer: here every view of the newest block is
/// dropped from otherwise built slots.
#[tokio::test]
async fn a_built_extension_missing_a_view_fails_every_read() {
    let chain = chain();
    let tip = tip_height(&chain);
    let harness = Harness::new(chain.clone(), 20, tip - 18).await;
    let before = harness.snapshot();
    harness.extend_and_advance(BlockSpec::spends(1)).await;
    let after = harness.snapshot();
    let mut slots = after.extensions().clone();
    slots.retain(&before);
    let mut broken = (*after).clone();
    broken.set_extensions(slots);

    let failed = |result: Result<(), ChainHeadError>| {
        matches!(result, Err(ChainHeadError::ReadFailed { .. }))
    };
    let newest = chain.tip().expect("a chain");
    let at = newest.height();
    let txid = newest.block.transactions()[0].txid;
    let outpoint = Outpoint { txid, index: 0 };
    assert!(failed(broken.transaction_locations(&txid).map(drop)));
    assert!(failed(broken.outpoint_spenders(&[outpoint]).map(drop)));
    assert!(failed(broken.txout_set().map(drop)));
    assert!(failed(
        broken
            .address_effects(&TransparentHistoryQuery {
                addresses: Vec::new(),
                start: height(0),
                end: at,
            })
            .map(drop)
    ));
    assert!(failed(
        broken.pool_frontier(ShieldedPool::Sapling, at).map(drop)
    ));
    assert!(failed(
        broken.subtree_roots(ShieldedPool::Sapling, 0, 1).map(drop)
    ));
    assert!(failed(broken.history_root(at).map(drop)));
    assert!(failed(broken.history_epoch(at).map(drop)));
    assert!(failed(broken.history_entries(at, &[0]).map(drop)));
}

/// What the extensions answer, against computations that use no index.
#[tokio::test]
async fn answers_match_independent_computations() {
    let chain = chain();
    let tip = tip_height(&chain);
    let harness = Harness::new(chain.clone(), 20, tip - 18).await;
    let snapshot = harness.snapshot();
    let root = harness.root();
    let window = window(&snapshot, root);
    assert_eq!(
        snapshot.capabilities().iter().collect::<Vec<_>>(),
        HeadCapability::ALL.to_vec()
    );

    // transactions: every window transaction located, every window spend named
    let mut spent_in_window: HashMap<Outpoint, (BlockRef, TransactionId, u32)> = HashMap::new();
    for block in &window {
        for (tx_index, tx) in (0u32..).zip(block.block.transactions()) {
            let located = snapshot.transaction_locations(&tx.txid).expect("built");
            let best = located.best_chain.expect("located");
            assert_eq!(best.block, block.reference);
            assert_eq!(best.tx_index, tx_index);
            assert!(located.non_best_chain.is_empty());
            for input in &tx.transparent.inputs {
                spent_in_window.insert(
                    Outpoint {
                        txid: input.prev_txid,
                        index: input.prev_index,
                    },
                    (block.reference, tx.txid, tx_index),
                );
            }
        }
    }
    assert!(
        spent_in_window.len() >= 6,
        "the window spends: {}",
        spent_in_window.len()
    );
    let below_window = chain.block_at(2).expect("held").block.transactions()[0].txid;
    assert_eq!(
        snapshot
            .transaction_locations(&below_window)
            .expect("built"),
        Default::default()
    );
    let mut outpoints: Vec<Outpoint> = spent_in_window.keys().copied().collect();
    outpoints.push(Outpoint {
        txid: below_window,
        index: 0,
    });
    let spenders = snapshot.outpoint_spenders(&outpoints).expect("built");
    for (outpoint, spender) in outpoints.iter().zip(&spenders) {
        match spent_in_window.get(outpoint) {
            Some((block, txid, tx_index)) => {
                let spender = spender.expect("spent in the window");
                assert_eq!(spender.block, *block);
                assert_eq!(spender.txid, *txid);
                assert_eq!(spender.tx_index, *tx_index);
            }
            None => assert!(spender.is_none()),
        }
    }

    // the UTXO set at the tip
    assert_eq!(
        snapshot.txout_set().expect("built"),
        independent_accumulator(&chain, tip)
    );

    // address effects for every address the window paid, over the window
    let mut expected_outputs: HashMap<TransparentAddressKey, Vec<Outpoint>> = HashMap::new();
    let mut expected_spends: HashMap<TransparentAddressKey, Vec<Outpoint>> = HashMap::new();
    let mut created: HashMap<Outpoint, TransparentOutput> = HashMap::new();
    for mock in blocks_through(&chain, tip) {
        for tx in mock.block.transactions() {
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                created.insert(
                    Outpoint {
                        txid: tx.txid,
                        index,
                    },
                    output.clone(),
                );
            }
        }
    }
    for block in &window {
        for tx in block.block.transactions() {
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                if let Some(address) =
                    TransparentAddressKey::from_standard_script(output.script.as_bytes())
                {
                    expected_outputs.entry(address).or_default().push(Outpoint {
                        txid: tx.txid,
                        index,
                    });
                }
            }
            for input in &tx.transparent.inputs {
                let outpoint = Outpoint {
                    txid: input.prev_txid,
                    index: input.prev_index,
                };
                let output = &created[&outpoint];
                if let Some(address) =
                    TransparentAddressKey::from_standard_script(output.script.as_bytes())
                {
                    expected_spends.entry(address).or_default().push(outpoint);
                }
            }
        }
    }
    assert!(
        !expected_spends.is_empty(),
        "the window spends standard outputs"
    );
    let mut addresses: Vec<TransparentAddressKey> = expected_outputs
        .keys()
        .chain(expected_spends.keys())
        .copied()
        .collect();
    addresses.sort_unstable_by_key(|address| address.hash);
    addresses.dedup();
    for address in addresses {
        let effects = snapshot
            .address_effects(&TransparentHistoryQuery {
                addresses: vec![address],
                start: height(0),
                end: height(tip),
            })
            .expect("built");
        let mut outputs: Vec<Outpoint> = effects
            .outputs
            .iter()
            .map(|output| output.outpoint)
            .collect();
        let mut expected = expected_outputs.remove(&address).unwrap_or_default();
        outputs.sort_unstable_by_key(|outpoint| (<[u8; 32]>::from(outpoint.txid), outpoint.index));
        expected.sort_unstable_by_key(|outpoint| (<[u8; 32]>::from(outpoint.txid), outpoint.index));
        assert_eq!(outputs, expected, "outputs of {address:?}");
        for output in &effects.outputs {
            assert_eq!(output.address, address);
            assert_eq!(created[&output.outpoint].value, output.value);
            assert_eq!(created[&output.outpoint].script, output.script);
        }
        let mut spends: Vec<Outpoint> = effects.spends.iter().map(|spend| spend.outpoint).collect();
        let mut expected = expected_spends.remove(&address).unwrap_or_default();
        spends.sort_unstable_by_key(|outpoint| (<[u8; 32]>::from(outpoint.txid), outpoint.index));
        expected.sort_unstable_by_key(|outpoint| (<[u8; 32]>::from(outpoint.txid), outpoint.index));
        assert_eq!(spends, expected, "spends of {address:?}");
        for spend in &effects.spends {
            assert_eq!(spend.address, address);
            assert_eq!(created[&spend.outpoint].value, spend.value);
        }
    }

    // trees and the history tree at every window height
    let blocks = fold_blocks(&chain, tip);
    for block in &window {
        let h = u32::from(block.height());
        let through = &blocks[..=usize::try_from(h).expect("small")];
        let (_, trees) = fold_from::<tree_state::TreeState>(None, through);
        let trees = trees.expect("folded");
        for (slot, pool) in trees.pools.iter().zip(ShieldedPool::ALL) {
            assert_eq!(
                snapshot.pool_frontier(pool, block.height()).expect("built"),
                Some(frontier_of_tree(slot)),
                "{pool} at {h}"
            );
        }
        assert!(snapshot
            .subtree_roots(ShieldedPool::Sapling, 0, 8)
            .expect("built")
            .is_empty());
        let (backend, history) = fold_from::<history_tree::HistoryTree>(None, through);
        let epoch = history.expect("folded").epoch.expect("an epoch");
        assert_eq!(
            snapshot.history_root(block.height()).expect("built"),
            Some(history_tree::root_of(&epoch).expect("a root")),
            "history root at {h}"
        );
        let at = snapshot
            .history_epoch(block.height())
            .expect("built")
            .expect("an epoch");
        assert_eq!(u32::from(at.branch), epoch.branch);
        assert_eq!(at.length, epoch.length);
        let backend_reader = backend.reader().expect("reader");
        let reader = history_tree::Reader::new(&backend_reader);
        let seed_length = folded::<history_tree::HistoryTree>(&chain, u32::from(root.height))
            .epoch
            .expect("an epoch")
            .length;
        let positions: Vec<u32> = (0..epoch.length).collect();
        let entries = snapshot
            .history_entries(block.height(), &positions)
            .expect("built");
        for (position, entry) in positions.iter().zip(entries) {
            match *position < seed_length {
                true => assert!(entry.is_none(), "position {position} is the store's"),
                false => assert_eq!(
                    entry.expect("appended in the window"),
                    reader.entry(epoch.start, *position).expect("stored"),
                    "entry {position} at {h}"
                ),
            }
        }
    }
    assert!(
        snapshot
            .pool_frontier(ShieldedPool::Sapling, height(u32::from(root.height) - 1))
            .expect("built")
            .is_none(),
        "below the window is not the head's"
    );
}

/// A reorg: the trunk's answers follow the new chain, the displaced blocks
/// keep their views and are reported off the best chain.
#[tokio::test]
async fn a_reorg_keeps_branch_views_and_answers_follow_the_trunk() {
    let chain = chain();
    let tip = tip_height(&chain);
    let harness = Harness::new(chain.clone(), 20, tip - 18).await;
    let before = harness.snapshot();
    // the spends block three below the tip: its spend transactions are its
    // own, unlike a coinbase the replacing block at that height repeats
    let displaced = chain.block_at(tip - 3).expect("held");
    let displaced_txid = displaced.block.transactions()[1].txid;

    let fork_parent = chain.block_at(tip - 4).expect("held");
    let activations = Activations::default();
    let mut branch = Vec::new();
    let mut parent = fork_parent;
    for seed in 1..=5u64 {
        let block = next_block(
            Some(&parent),
            &activations,
            BlockSpec::spends(1).seeded(seed),
        );
        parent = block.clone();
        branch.push(block);
    }
    chain.reorg(tip - 3, branch);
    harness.service.advance_once().await.expect("advances");
    let after = harness.snapshot();

    assert_eq!(u32::from(after.best_tip().height), tip + 1);
    assert!(
        after.extension_failures().is_empty(),
        "{:?}",
        after.extension_failures()
    );
    let located = after.transaction_locations(&displaced_txid).expect("built");
    assert!(located.best_chain.is_none());
    assert_eq!(located.non_best_chain.len(), 1);
    assert_eq!(located.non_best_chain[0].block, reference(&displaced));
    assert_eq!(
        after.txout_set().expect("built"),
        independent_accumulator(&chain, tip + 1)
    );
    assert_ne!(
        after.txout_set().expect("built"),
        before.txout_set().expect("built")
    );
    // every trunk transaction is located on the trunk (a branch block may
    // repeat a trunk transaction, which is then the trunk's)
    for block in window(&after, harness.root())
        .into_iter()
        .filter(|block| after.is_trunk_block(block.reference))
    {
        for tx in block.block.transactions() {
            assert_eq!(
                after
                    .transaction_locations(&tx.txid)
                    .expect("built")
                    .best_chain
                    .map(|at| at.block),
                Some(block.reference)
            );
        }
    }
}

/// A port that fails darkens the extensions that asked it, the others keep
/// answering, and the next tick rebuilds the failed ones. The ports are asked
/// at a rebase (the root moved far enough), so the three seeded extensions
/// fail and the two seedless ones do not.
#[tokio::test]
async fn a_failing_port_darkens_one_extension_and_the_next_tick_rebuilds_it() {
    let chain = chain();
    let tip = tip_height(&chain);
    let floor = tip - 60;
    let harness = Harness::new(chain.clone(), 60, floor).await;

    harness.below.failing.store(true, Ordering::SeqCst);
    let new_root = chain.block_at(floor + 40).expect("held");
    harness.below.set_tip(reference(&new_root));
    harness.extend_and_advance(BlockSpec::default()).await;
    let dark = harness.snapshot();
    let failed: Vec<HeadCapability> = dark
        .extension_failures()
        .into_iter()
        .map(|(capability, _)| capability)
        .collect();
    assert_eq!(
        failed,
        vec![
            HeadCapability::TxOutSet,
            HeadCapability::TreeState,
            HeadCapability::HistoryTree
        ],
        "the three seeded below the root"
    );
    let not_built =
        |result: Result<(), ChainHeadError>| matches!(result, Err(ChainHeadError::NotBuilt { .. }));
    assert!(not_built(dark.txout_set().map(drop)));
    assert!(not_built(dark.history_root(height(tip)).map(drop)));
    assert!(not_built(
        dark.pool_frontier(ShieldedPool::Sapling, height(tip))
            .map(drop)
    ));
    let newest = chain.tip().expect("a chain");
    assert!(
        dark.transaction_locations(&newest.block.transactions()[0].txid)
            .expect("built")
            .best_chain
            .is_some(),
        "transactions still answers"
    );
    assert!(
        dark.address_effects(&TransparentHistoryQuery {
            addresses: Vec::new(),
            start: height(0),
            end: height(tip + 1),
        })
        .is_ok(),
        "address history still answers"
    );

    harness.below.failing.store(false, Ordering::SeqCst);
    harness.extend_and_advance(BlockSpec::default()).await;
    let rebuilt = harness.snapshot();
    assert!(
        rebuilt.extension_failures().is_empty(),
        "{:?}",
        rebuilt.extension_failures()
    );
    assert_eq!(
        rebuilt.txout_set().expect("rebuilt"),
        independent_accumulator(&chain, tip + 2)
    );
}

/// The root moves far enough for a rebase: no answer changes, the views now
/// sit on the new root.
#[tokio::test]
async fn a_rebase_leaves_every_answer_unchanged() {
    let chain = chain();
    let tip = tip_height(&chain);
    let floor = tip - 60;
    let harness = Harness::new(chain.clone(), 60, floor).await;
    let before = harness.snapshot();
    assert!(before.extension_failures().is_empty());

    let new_root = chain.block_at(floor + 40).expect("held");
    harness.below.set_tip(reference(&new_root));
    harness.extend_and_advance(BlockSpec::spends(1)).await;
    let after = harness.snapshot();
    let tip = tip + 1;

    assert!(
        after.extension_failures().is_empty(),
        "{:?}",
        after.extension_failures()
    );
    let slots = after.extensions();
    let rooted = slots
        .txout_set
        .as_ref()
        .and_then(|slot| slot.built())
        .map(|built| built.root())
        .expect("built");
    assert_eq!(rooted, reference(&new_root), "rebased onto the new root");
    assert_eq!(
        after.txout_set().expect("built"),
        independent_accumulator(&chain, tip)
    );
    let blocks = fold_blocks(&chain, tip);
    for block in window(&after, reference(&new_root)) {
        let h = u32::from(block.height());
        let through = &blocks[..=usize::try_from(h).expect("small")];
        let (_, history) = fold_from::<history_tree::HistoryTree>(None, through);
        let epoch = history.expect("folded").epoch.expect("an epoch");
        assert_eq!(
            after.history_root(block.height()).expect("built"),
            Some(history_tree::root_of(&epoch).expect("a root"))
        );
        let (_, trees) = fold_from::<tree_state::TreeState>(None, through);
        for (slot, pool) in trees.expect("folded").pools.iter().zip(ShieldedPool::ALL) {
            assert_eq!(
                after.pool_frontier(pool, block.height()).expect("built"),
                Some(frontier_of_tree(slot))
            );
        }
        for (tx_index, tx) in (0u32..).zip(block.block.transactions()) {
            let best = after
                .transaction_locations(&tx.txid)
                .expect("built")
                .best_chain
                .expect("located");
            assert_eq!((best.block, best.tx_index), (block.reference, tx_index));
        }
    }
    assert!(
        after
            .pool_frontier(ShieldedPool::Sapling, new_root.height())
            .expect("built")
            .is_none(),
        "the root itself is the store's"
    );
}
