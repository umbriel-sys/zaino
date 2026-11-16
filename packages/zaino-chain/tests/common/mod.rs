//! The real services under the real composer (D-90): the shared fake
//! validator (`MockChain`), the store over in-memory storage, and the chain
//! head, stepped unless a test asks for its writer.
//!
//! A [`Rig`] is one coverage shape: the store built to a height (or not), the
//! head's window `floor..=tip`, the validator serving to `tip`. Chains are
//! built once per process and shared.

#![allow(dead_code)]

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use tokio_util::sync::CancellationToken;
use zaino_chain::{ChainViewComposer, ChainViewComposerBuilder, ChainViewConfig};
use zaino_chain_head::ChainHeadConfig;
use zaino_chain_head_service::{ChainHeadService, ChainHeadSubscriber};
use zaino_chain_store_service::{
    ChainStore, ChainStoreServiceConfig, IndexesConfig, IngestConfig, NamedSet, OnCorruption,
};
use zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH;
use zaino_persistence::in_memory::InMemoryStorage;
use zaino_primitives::types::{
    AbsoluteChainWork, BlockHash, Height, Network, NetworkUpgradeInfo, Outpoint, ShieldedPool,
    Transaction, TransactionId,
};
use zaino_source_mock::{
    Activations, BlockSpec, Call, CallArg, ChainBuilder, MockBlock, MockChain, NodeFacts,
};

/// The fast chain's tip: genesis plus 300 blocks.
pub const TIP: u32 = 300;
/// The fast seam depth, one tenth of the protocol's.
pub const DEPTH: u32 = FAST_TEST_MAX_NONFINALISED_DEPTH;
/// The long chain's tip, deeper than the protocol's seam.
pub const LONG_TIP: u32 = 1200;

/// The store, as the suites run it.
pub type Store = ChainStore<InMemoryStorage, MockChain>;
/// The composer, as the suites run it.
pub type View = ChainViewComposer<Store, ChainHeadSubscriber, MockChain>;
/// The composer's builder, as the suites run it.
pub type ViewBuilder = ChainViewComposerBuilder<Store, ChainHeadSubscriber, MockChain>;

/// Every block spends two of its parent's outputs once coinbases mature, and
/// every tenth adds Sapling and Orchard notes.
fn blocks(tip: u32) -> Vec<MockBlock> {
    (1..=tip)
        .fold(ChainBuilder::new(), |builder, height| {
            let spec = BlockSpec::spends(2);
            builder.block(match height % 10 {
                0 => spec
                    .notes(ShieldedPool::Sapling, 2)
                    .notes(ShieldedPool::Orchard, 2),
                _ => spec,
            })
        })
        .into_blocks()
}

fn shared(cell: &'static OnceLock<Vec<MockBlock>>, tip: u32) -> MockChain {
    MockChain::new(
        NodeFacts::regtest(),
        cell.get_or_init(|| blocks(tip)).clone(),
    )
}

/// A fresh fake validator over the fast chain, serving all of it.
pub fn chain() -> MockChain {
    static BLOCKS: OnceLock<Vec<MockBlock>> = OnceLock::new();
    shared(&BLOCKS, TIP)
}

/// A fresh fake validator over the long chain, serving all of it.
pub fn long_chain() -> MockChain {
    static BLOCKS: OnceLock<Vec<MockBlock>> = OnceLock::new();
    shared(&BLOCKS, LONG_TIP)
}

/// The fast chain served by a node reporting `facts`.
pub fn chain_with_facts(facts: NodeFacts) -> MockChain {
    let default = chain();
    let blocks = (0..=TIP).map(|h| block_at(&default, h)).collect();
    MockChain::new(facts, blocks)
}

/// The calls the validator answered after the first `from`.
pub fn calls_after(chain: &MockChain, from: usize) -> Vec<Call> {
    chain.call_log().into_iter().skip(from).collect()
}

/// Whether `calls` holds a call of `port` with `arg`.
pub fn called(calls: &[Call], port: &str, arg: &CallArg) -> bool {
    calls
        .iter()
        .any(|call| call.port == port && call.arg == *arg)
}

/// Every transparent outpoint the chain created through `through`, and who
/// spent each, from the blocks themselves.
pub fn ledger(
    chain: &MockChain,
    through: u32,
) -> (Vec<Outpoint>, HashMap<Outpoint, TransactionId>) {
    let mut created = Vec::new();
    let mut spenders = HashMap::new();
    for h in 0..=through {
        for tx in block_at(chain, h).block.transactions() {
            for input in &tx.transparent.inputs {
                spenders.insert(
                    Outpoint {
                        txid: input.prev_txid,
                        index: input.prev_index,
                    },
                    tx.txid,
                );
            }
            for index in 0..tx.transparent.outputs.len() {
                created.push(Outpoint {
                    txid: tx.txid,
                    index: index as u32,
                });
            }
        }
    }
    (created, spenders)
}

/// The consensus bytes of the best-chain block at `h`.
pub fn raw_of(chain: &MockChain, h: u32) -> Vec<u8> {
    block_at(chain, h)
        .raw
        .expect("mock blocks carry their bytes")
        .block
}

/// The chains' upgrade schedule, as the store takes it.
pub fn schedule() -> Arc<[NetworkUpgradeInfo]> {
    Arc::from(Activations::default().upgrades())
}

pub fn height(h: u32) -> Height {
    Height::try_from(h).expect("a test height is in range")
}

/// The best-chain block at `h`.
pub fn block_at(chain: &MockChain, h: u32) -> MockBlock {
    chain.block_at(h).expect("the chain holds this height")
}

pub fn hash_of(chain: &MockChain, h: u32) -> BlockHash {
    block_at(chain, h).hash()
}

/// The transaction at `index` of the block at `h`.
pub fn tx_at(chain: &MockChain, h: u32, index: usize) -> Transaction {
    block_at(chain, h)
        .block
        .transactions()
        .get(index)
        .expect("the block holds this transaction")
        .clone()
}

/// Cumulative work through `h`: every header's work accumulated from genesis.
pub fn chainwork_at(chain: &MockChain, h: u32) -> AbsoluteChainWork {
    (1..=h).fold(
        AbsoluteChainWork::genesis(block_at(chain, 0).block.header.bits.to_work()),
        |work, at| {
            work.accumulate(block_at(chain, at).block.header.bits.to_work())
                .expect("test chains do not overflow")
        },
    )
}

/// `count` blocks on top of the best-chain block at `parent`, on a branch
/// named by `seed`.
pub fn branch(chain: &MockChain, parent: u32, count: u32, seed: u64) -> Vec<MockBlock> {
    let activations = Activations::default();
    let mut parent = block_at(chain, parent);
    let mut blocks = Vec::new();
    for _ in 0..count {
        let block = zaino_source_mock::next_block(
            Some(&parent),
            &activations,
            BlockSpec::spends(1).seeded(seed),
        );
        parent = block.clone();
        blocks.push(block);
    }
    blocks
}

/// Replaces the best chain from `from` with `count` blocks of branch `seed`.
pub fn reorg(chain: &MockChain, from: u32, count: u32, seed: u64) {
    let blocks = branch(chain, from.saturating_sub(1), count, seed);
    chain.reorg(from, blocks);
}

/// The store's configuration over `indexes`.
fn store_config(indexes: IndexesConfig) -> ChainStoreServiceConfig {
    ChainStoreServiceConfig {
        root: PathBuf::from("/zaino-chain-tests"),
        network: Network::Regtest,
        indexes,
        on_corruption: OnCorruption::default(),
        ingest: IngestConfig::default(),
    }
}

/// Every index.
pub fn every_index() -> IndexesConfig {
    IndexesConfig {
        set: Some(NamedSet::Full),
        ..IndexesConfig::default()
    }
}

/// Every index but `disabled`.
pub fn without_indexes(disabled: &[&str]) -> IndexesConfig {
    IndexesConfig {
        set: Some(NamedSet::Full),
        disable: disabled.iter().map(|name| (*name).to_owned()).collect(),
        ..IndexesConfig::default()
    }
}

/// Only `enabled` (and what they depend on).
pub fn only_indexes(enabled: &[&str]) -> IndexesConfig {
    IndexesConfig {
        enable: enabled.iter().map(|name| (*name).to_owned()).collect(),
        ..IndexesConfig::default()
    }
}

/// One coverage shape, assembled.
pub struct Rig {
    pub chain: MockChain,
    pub store: Store,
    pub head: Arc<ChainHeadService<MockChain>>,
}

impl Rig {
    /// A composer over the three, with `config`.
    pub fn view_with(&self, config: ChainViewConfig) -> View {
        self.view_over(self.chain.clone(), config)
    }

    /// A composer over the store, the head and `source`: a second validator
    /// over the same chain, whose call log and in-flight counters hold only
    /// what the composer asked.
    pub fn view_over(&self, source: MockChain, config: ChainViewConfig) -> View {
        ChainViewComposer::new(
            self.store.clone(),
            self.head.subscriber(),
            Arc::new(source),
            config,
        )
    }

    /// The head's floor now: its lowest retained canonical block.
    pub fn head_floor(&self) -> u32 {
        use zaino_chain_head::{ChainHeadBlockService as _, ChainHeadSnapshot as _};
        let snapshot = self.head.subscriber().current();
        let floor = snapshot.best_chain().next().expect("a window").height();
        u32::from(floor)
    }

    /// The store's watermark height now.
    pub fn store_top(&self) -> Option<u32> {
        self.store
            .subscribe_watermark()
            .borrow()
            .tip
            .map(|tip| u32::from(tip.height))
    }

    /// Appends `count` blocks to the chain's best chain (served at once).
    pub fn extend(&self, count: u32) {
        let tip = self
            .chain
            .tip()
            .map_or(0, |block| u32::from(block.height()));
        for block in branch(&self.chain, tip, count, 0) {
            self.chain.extend(block);
        }
    }

    /// A composer over the three.
    pub fn view(&self) -> View {
        self.view_with(ChainViewConfig::default())
    }

    /// The composer's builder over the three.
    pub fn builder(&self) -> ViewBuilder {
        ChainViewComposer::builder(
            self.store.clone(),
            self.head.subscriber(),
            Arc::new(self.chain.clone()),
        )
    }

    /// Advances the stepped head once, to the validator's served tip.
    pub async fn advance(&self) {
        self.head.advance_once().await.expect("the head advances");
    }
}

/// How a [`Rig`] is shaped.
pub struct Shape {
    chain: MockChain,
    store_to: Option<u32>,
    indexes: IndexesConfig,
    floor: Option<u32>,
    tip: Option<u32>,
    running: bool,
}

/// A rig over `chain`: the store unbuilt, the head's window the fast depth
/// below the chain's tip, the head stepped.
pub fn rig(chain: MockChain) -> Shape {
    Shape {
        chain,
        store_to: None,
        indexes: every_index(),
        floor: None,
        tip: None,
        running: false,
    }
}

impl Shape {
    /// The store built through `top`.
    pub fn store_to(mut self, top: u32) -> Self {
        self.store_to = Some(top);
        self
    }

    /// The store's indexes.
    pub fn indexes(mut self, indexes: IndexesConfig) -> Self {
        self.indexes = indexes;
        self
    }

    /// The head's window `floor..=tip`; the validator serves through `tip`.
    pub fn window(mut self, floor: u32, tip: u32) -> Self {
        self.floor = Some(floor);
        self.tip = Some(tip);
        self
    }

    /// The head with its writer running (polling every few milliseconds)
    /// instead of stepped.
    pub fn running_head(mut self) -> Self {
        self.running = true;
        self
    }

    pub async fn build(self) -> Rig {
        let chain = self.chain;
        let chain_tip = chain.tip().map_or(0, |block| u32::from(block.height()));
        let tip = self.tip.unwrap_or(chain_tip);
        let floor = self.floor.unwrap_or(tip.saturating_sub(DEPTH));
        chain.serve_to(tip);

        let store = ChainStore::boot(
            &store_config(self.indexes),
            InMemoryStorage::new(),
            Arc::new(chain.clone()),
            schedule(),
        )
        .await
        .expect("the store boots over in-memory storage");
        if let Some(top) = self.store_to {
            store.build_to(height(top)).await.expect("the store builds");
        }

        let depth = NonZeroU32::new(tip.saturating_sub(floor).max(1)).expect("non-zero");
        let mut config = ChainHeadConfig::with_max_depth(depth);
        let source = Arc::new(chain.clone());
        let head = match self.running {
            false => {
                let head = ChainHeadService::spawn_without_writer(
                    source,
                    config,
                    CancellationToken::new(),
                )
                .await
                .expect("the head anchors");
                head.advance_once()
                    .await
                    .expect("the head fills its window");
                head
            }
            true => {
                config.set_poll_interval_ms(std::num::NonZeroU64::new(2).expect("non-zero"));
                ChainHeadService::spawn(source, config, CancellationToken::new())
                    .await
                    .expect("the head anchors")
            }
        };
        Rig { chain, store, head }
    }
}

/// Waits for `condition`, failing in bounded time.
pub async fn eventually(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if condition() {
            return true;
        }
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    false
}
