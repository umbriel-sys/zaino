//! The chain the mock serves, and everything a test controls about it.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::watch;
use zaino_primitives::types::{
    BlockHash, Height, Network, Transaction, TransactionId, TransactionLocation,
};
use zaino_source::{FailureMode, NonDomainError, QueryError, SendRawTransactionError};

use crate::block::MockBlock;

/// The mock's mempool.
#[derive(Debug, Clone)]
pub enum Mempool {
    /// The next best-chain block past the mempool height, without its
    /// coinbase: what mining it would confirm.
    NextBlock,
    /// Exactly these transactions, with their consensus bytes where known.
    Explicit(Vec<(Transaction, Option<Vec<u8>>)>),
}

/// Failures a test arms.
#[derive(Debug, Clone)]
struct Fault {
    /// The ports it hits; `None` = every port.
    ports: Option<HashSet<&'static str>>,
    /// Calls left to fail; `None` = until cleared.
    remaining: Option<u32>,
    mode: FailureMode,
}

/// Node facts the ports report beside the chain: what a validator knows
/// about itself rather than derives from blocks.
#[derive(Debug, Clone)]
pub struct NodeFacts {
    /// The network kind.
    pub network: Network,
    /// The activation schedule (status is derived from the tip).
    pub upgrades: Vec<zaino_primitives::types::NetworkUpgradeInfo>,
    /// `getinfo`'s version.
    pub version: u64,
    /// `getinfo`'s build.
    pub build: String,
    /// `getinfo`'s subversion.
    pub subversion: String,
    /// `getinfo`'s protocol version.
    pub protocol_version: u32,
    /// The connected peers.
    pub peers: Vec<zaino_primitives::types::rpc::PeerInfo>,
    /// `getinfo`'s pay and relay fees.
    pub pay_tx_fee: zaino_primitives::types::Zatoshis,
    /// The relay fee.
    pub relay_fee: zaino_primitives::types::Zatoshis,
    /// The estimated network solution rate.
    pub network_sol_ps: u64,
    /// `getblockchaininfo`'s size on disk.
    pub size_on_disk: u64,
    /// The subsidy answered for every height up to the tip; `None` = a fault.
    pub subsidy: Option<zaino_primitives::types::rpc::BlockSubsidy>,
    /// The standard fee; `None` = `Unsupported` (a validator without it).
    pub standard_fee: Option<zaino_primitives::types::rpc::StandardFee>,
    /// Whether chain work is answered, as zcashd does; zebra answers none.
    pub chainwork: bool,
    /// The pools `z_getsubtreesbyindex` answers: all three, as zebra; zcashd
    /// lacks Ironwood.
    pub subtree_pools: Vec<zaino_primitives::types::ShieldedPool>,
}

/// A port call's arguments, as the mock received them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallArg {
    /// No arguments.
    None,
    /// A height.
    Height(Height),
    /// A block hash.
    Hash(BlockHash),
    /// A transaction id.
    Txid(TransactionId),
    /// Addresses, with the height range where the port takes one.
    Addresses {
        /// The addresses.
        addresses: Vec<String>,
        /// The range, inclusive.
        range: Option<(Height, Height)>,
    },
    /// A `gettxout` query.
    TxOut {
        /// The transaction.
        txid: TransactionId,
        /// The output.
        index: zaino_primitives::types::OutputIndex,
        /// Whether mempool spends count.
        include_mempool: bool,
    },
    /// An outpoint.
    Outpoint(zaino_primitives::types::rpc::SpentOutpoint),
    /// A subtree roots query.
    Subtrees {
        /// The pool.
        pool: zaino_primitives::types::ShieldedPool,
        /// The first subtree.
        start_index: u16,
        /// The most answered.
        limit: Option<u16>,
    },
    /// A solution rate query.
    SolPs {
        /// The averaging window.
        blocks: Option<u32>,
        /// The height.
        height: Option<Height>,
    },
    /// Transaction bytes.
    Bytes(Vec<u8>),
}

/// One port call, in the order received.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    /// The port's method name.
    pub port: &'static str,
    /// Its arguments.
    pub arg: CallArg,
}

impl NodeFacts {
    /// A regtest node on the default schedule, with no peers.
    pub fn regtest() -> Self {
        Self::of(Network::Regtest)
    }

    /// A node on `network` on the default schedule, with no peers.
    pub fn of(network: Network) -> Self {
        Self {
            network,
            upgrades: crate::Activations::default().upgrades(),
            version: 6_000_000,
            build: "mock".to_owned(),
            subversion: "/MockChain:0.1.0/".to_owned(),
            protocol_version: 170_120,
            peers: Vec::new(),
            pay_tx_fee: zaino_primitives::types::Zatoshis::ZERO,
            relay_fee: zaino_primitives::types::Zatoshis::ZERO,
            network_sol_ps: 0,
            size_on_disk: 0,
            subsidy: None,
            standard_fee: None,
            chainwork: true,
            subtree_pools: zaino_primitives::types::ShieldedPool::ALL.to_vec(),
        }
    }
}

struct State {
    facts: NodeFacts,
    /// Every block ever known, canonical or not.
    known: HashMap<BlockHash, Arc<MockBlock>>,
    /// The best chain, by height.
    best: Vec<BlockHash>,
    /// The highest best-chain height served (mined); `None` = all of it.
    served: Option<u32>,
    /// The height the mempool ports serve at, while held behind.
    mempool_held: Option<u32>,
    mempool: Mempool,
    /// Where each txid occurs: block and position.
    txids: HashMap<TransactionId, Vec<(BlockHash, u32)>>,
    faults: Vec<Fault>,
    stale_roots: usize,
    calls: Vec<Call>,
    shutdown: bool,
    sent: Vec<Vec<u8>>,
    scripted: HashMap<Vec<u8>, Result<TransactionId, SendRawTransactionError>>,
}

/// A deterministic fake validator: one chain, every port.
///
/// Cheap to clone; clones share the chain, so a test keeps one to drive
/// (extend, reorg, mine, arm failures) while the code under test holds
/// another.
#[derive(Clone)]
pub struct MockChain {
    state: Arc<Mutex<State>>,
    blocks_received: Arc<watch::Sender<()>>,
    /// Calls in flight now, and the most ever at once.
    in_flight: Arc<AtomicUsize>,
    peak_in_flight: Arc<AtomicUsize>,
}

/// A port's view of the state for one call.
pub(crate) struct Served<'a> {
    state: MutexGuard<'a, State>,
    _in_flight: InFlight,
}

/// Counts one call in flight until dropped.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl MockChain {
    /// `blocks` as the best chain from genesis, all served.
    pub fn new(facts: NodeFacts, blocks: Vec<MockBlock>) -> Self {
        let mut state = State {
            facts,
            known: HashMap::new(),
            best: Vec::new(),
            served: None,
            mempool_held: None,
            mempool: Mempool::NextBlock,
            txids: HashMap::new(),
            faults: Vec::new(),
            stale_roots: 0,
            calls: Vec::new(),
            shutdown: false,
            sent: Vec::new(),
            scripted: HashMap::new(),
        };
        for block in blocks {
            state.push_best(block);
        }
        Self {
            state: Arc::new(Mutex::new(state)),
            blocks_received: Arc::new(watch::channel(()).0),
            in_flight: Arc::new(AtomicUsize::new(0)),
            peak_in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Serves only the best chain up to `height`; later blocks arrive by
    /// [`Self::mine`].
    pub fn serve_to(&self, height: u32) {
        self.lock().served = Some(height);
    }

    /// Serves up to `blocks` more of the loaded best chain and wakes block
    /// subscribers if anything was served.
    pub fn mine(&self, blocks: u32) {
        if self.advance(blocks) {
            self.blocks_received.send_replace(());
        }
    }

    /// [`Self::mine`] without waking subscribers.
    pub fn mine_silent(&self, blocks: u32) {
        self.advance(blocks);
    }

    /// [`Self::mine`], the mempool ports still serving the pre-mine tip.
    pub fn mine_ahead_of_mempool(&self, blocks: u32) {
        {
            let mut state = self.lock();
            let tip = state.served_tip();
            state.mempool_held.get_or_insert(tip);
        }
        self.mine(blocks);
    }

    /// Lets the mempool ports catch up with the served tip.
    pub fn release_mempool(&self) {
        self.lock().mempool_held = None;
    }

    fn advance(&self, blocks: u32) -> bool {
        let mut state = self.lock();
        let last = state.last_height();
        let current = state.served_tip();
        let target = current.saturating_add(blocks).min(last);
        let moved = target != current;
        state.served = Some(target);
        moved
    }

    /// Appends `block` to the best chain (served at once unless serving is
    /// held by [`Self::serve_to`]).
    pub fn extend(&self, block: MockBlock) {
        let mut state = self.lock();
        let held = state
            .served
            .is_some_and(|served| served < state.last_height());
        state.push_best(block);
        if !held {
            state.served = None;
        }
    }

    /// Replaces the best chain from `height` with `blocks`; the displaced
    /// blocks stay known, off the best chain.
    pub fn reorg(&self, height: u32, blocks: Vec<MockBlock>) {
        let mut state = self.lock();
        let keep = usize::try_from(height).unwrap_or(usize::MAX);
        state.best.truncate(keep);
        for block in blocks {
            state.push_best(block);
        }
        state.served = None;
    }

    /// Makes `block` known off the best chain: a side block, served by hash.
    pub fn know(&self, block: MockBlock) {
        self.lock().insert_known(block);
    }

    /// The current best block at `height`, served or not.
    pub fn block_at(&self, height: u32) -> Option<MockBlock> {
        let state = self.lock();
        let hash = state.best.get(usize::try_from(height).ok()?)?;
        state.known.get(hash).map(|block| (**block).clone())
    }

    /// The served tip.
    pub fn tip(&self) -> Option<MockBlock> {
        let state = self.lock();
        let height = state.served_tip();
        drop(state);
        self.block_at(height)
    }

    /// Sets the mempool.
    pub fn set_mempool(&self, mempool: Mempool) {
        self.lock().mempool = mempool;
    }

    /// Every port fails with a connection error until cleared.
    pub fn set_failing(&self, failing: bool) {
        let mut state = self.lock();
        state.faults.clear();
        if failing {
            state.faults.push(Fault {
                ports: None,
                remaining: None,
                mode: FailureMode::Connection,
            });
        }
    }

    /// The next `count` calls to any port fail with `mode`.
    pub fn fail_next(&self, count: u32, mode: FailureMode) {
        self.lock().faults.push(Fault {
            ports: None,
            remaining: Some(count),
            mode,
        });
    }

    /// The next `count` calls to `ports` fail with `mode`.
    pub fn fail_ports(&self, ports: &[&'static str], count: Option<u32>, mode: FailureMode) {
        self.lock().faults.push(Fault {
            ports: Some(ports.iter().copied().collect()),
            remaining: count,
            mode,
        });
    }

    /// The next `count` by-height roots answers name a block no one has.
    pub fn answer_stale_roots(&self, count: usize) {
        self.lock().stale_roots = count;
    }

    /// How many times `port` (its method name) was called.
    pub fn calls(&self, port: &str) -> usize {
        self.lock()
            .calls
            .iter()
            .filter(|call| call.port == port)
            .count()
    }

    /// Every call, in the order received.
    pub fn call_log(&self) -> Vec<Call> {
        self.lock().calls.clone()
    }

    /// The most calls ever in flight at once.
    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Acquire)
    }

    /// Whether `shutdown` ran.
    pub fn shutdown_called(&self) -> bool {
        self.lock().shutdown
    }

    /// What `send_raw_transaction` answers for `transaction`.
    pub fn script_send(
        &self,
        transaction: Vec<u8>,
        answer: Result<TransactionId, SendRawTransactionError>,
    ) {
        self.lock().scripted.insert(transaction, answer);
    }

    /// Every transaction `send_raw_transaction` received, in order.
    pub fn sent(&self) -> Vec<Vec<u8>> {
        self.lock().sent.clone()
    }

    /// One call to `port`: logged, failed if a fault is armed for it, else
    /// in flight until the view drops. Yields first, so concurrent calls
    /// overlap.
    pub(crate) async fn enter<E>(
        &self,
        port: &'static str,
        arg: CallArg,
    ) -> Result<Served<'_>, QueryError<E>>
    where
        E: std::fmt::Debug + std::fmt::Display,
    {
        let fault = {
            let mut state = self.lock();
            state.calls.push(Call { port, arg });
            state.take_fault(port)
        };
        if let Some(mode) = fault {
            return Err(QueryError::NonDomain(NonDomainError::new(
                mode,
                format!("mock: {port} failed as armed"),
            )));
        }
        let now = self
            .in_flight
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        self.peak_in_flight.fetch_max(now, Ordering::AcqRel);
        let in_flight = InFlight(Arc::clone(&self.in_flight));
        tokio::task::yield_now().await;
        Ok(Served {
            state: self.lock(),
            _in_flight: in_flight,
        })
    }

    pub(crate) fn wake(&self) -> watch::Receiver<()> {
        self.blocks_received.subscribe()
    }

    pub(crate) fn record_shutdown(&self) {
        self.lock().shutdown = true;
    }
}

impl State {
    fn push_best(&mut self, block: MockBlock) {
        let hash = self.insert_known(block);
        self.best.push(hash);
    }

    fn insert_known(&mut self, block: MockBlock) -> BlockHash {
        let hash = block.hash();
        for (position, tx) in (0u32..).zip(block.block.transactions()) {
            let places = self.txids.entry(tx.txid).or_default();
            if !places.contains(&(hash, position)) {
                places.push((hash, position));
            }
        }
        self.known.insert(hash, Arc::new(block));
        hash
    }

    fn last_height(&self) -> u32 {
        u32::try_from(self.best.len().saturating_sub(1)).unwrap_or(u32::MAX)
    }

    fn served_tip(&self) -> u32 {
        let last = self.last_height();
        self.served.map_or(last, |served| served.min(last))
    }

    fn take_fault(&mut self, port: &'static str) -> Option<FailureMode> {
        let at = self.faults.iter().position(|fault| {
            fault
                .ports
                .as_ref()
                .is_none_or(|ports| ports.contains(port))
        })?;
        let fault = self.faults.get_mut(at)?;
        let mode = fault.mode.clone();
        match &mut fault.remaining {
            Some(remaining) if *remaining <= 1 => {
                self.faults.remove(at);
            }
            Some(remaining) => *remaining -= 1,
            None => {}
        }
        Some(mode)
    }
}

impl Served<'_> {
    /// The node facts.
    pub(crate) fn facts(&self) -> &NodeFacts {
        &self.state.facts
    }

    /// The served tip height; `None` before genesis is loaded.
    pub(crate) fn tip_height(&self) -> Option<u32> {
        (!self.state.best.is_empty()).then(|| self.state.served_tip())
    }

    /// The served best block at `height`.
    pub(crate) fn at(&self, height: Height) -> Option<&MockBlock> {
        let height = u32::from(height);
        if self.tip_height()? < height {
            return None;
        }
        let hash = self.state.best.get(usize::try_from(height).ok()?)?;
        self.state.known.get(hash).map(Arc::as_ref)
    }

    /// The served tip block.
    pub(crate) fn tip(&self) -> Option<&MockBlock> {
        self.at(Height::try_from(self.tip_height()?).ok()?)
    }

    /// A block the node knows by `hash`: any but best blocks not yet mined.
    pub(crate) fn by_hash(&self, hash: BlockHash) -> Option<&MockBlock> {
        let block = self.state.known.get(&hash)?;
        let unmined = self.is_best(hash) && self.at(block.height()).is_none();
        (!unmined).then_some(block.as_ref())
    }

    /// Whether `hash` is on the best chain (served or not).
    pub(crate) fn is_best(&self, hash: BlockHash) -> bool {
        self.state.known.get(&hash).is_some_and(|block| {
            usize::try_from(u32::from(block.height()))
                .ok()
                .and_then(|at| self.state.best.get(at))
                == Some(&hash)
        })
    }

    /// The served best block after `block`, if any.
    pub(crate) fn next_of(&self, block: &MockBlock) -> Option<&MockBlock> {
        if !self.is_best(block.hash()) {
            return None;
        }
        self.at(block.height().checked_add(1)?)
    }

    /// Served best blocks from genesis to the tip.
    pub(crate) fn best_chain(&self) -> impl Iterator<Item = &MockBlock> {
        let tip = self.tip_height();
        self.state
            .best
            .iter()
            .take(tip.map_or(0, |tip| {
                usize::try_from(tip).unwrap_or(usize::MAX).saturating_add(1)
            }))
            .filter_map(|hash| self.state.known.get(hash).map(Arc::as_ref))
    }

    /// Visible blocks off the best chain.
    pub(crate) fn side_blocks(&self) -> impl Iterator<Item = &MockBlock> {
        self.state
            .known
            .values()
            .filter(|block| !self.is_best(block.hash()))
            .map(Arc::as_ref)
    }

    /// Where `txid` is (best chain first, then side chains, then mempool),
    /// with its block and position when mined.
    pub(crate) fn locate(
        &self,
        txid: TransactionId,
    ) -> Option<(TransactionLocation, Option<(&MockBlock, u32)>)> {
        let places = self.state.txids.get(&txid);
        let visible = |(hash, position): &(BlockHash, u32)| {
            self.by_hash(*hash).map(|block| (block, *position))
        };
        if let Some(found) = places
            .into_iter()
            .flatten()
            .filter_map(visible)
            .find(|(block, _)| self.is_best(block.hash()))
        {
            return Some((
                TransactionLocation::BestChain(found.0.height()),
                Some(found),
            ));
        }
        if let Some(found) = places.into_iter().flatten().filter_map(visible).next() {
            return Some((TransactionLocation::NonBestChain, Some(found)));
        }
        self.mempool()
            .into_iter()
            .any(|(tx, _)| tx.txid == txid)
            .then_some((TransactionLocation::Mempool, None))
    }

    /// The height the mempool ports serve at.
    pub(crate) fn mempool_height(&self) -> Option<u32> {
        let tip = self.tip_height()?;
        Some(self.state.mempool_held.map_or(tip, |held| held.min(tip)))
    }

    /// The mempool's transactions with their bytes where known.
    pub(crate) fn mempool(&self) -> Vec<(Transaction, Option<Vec<u8>>)> {
        match &self.state.mempool {
            Mempool::Explicit(transactions) => transactions.clone(),
            Mempool::NextBlock => {
                let Some(next) = self
                    .mempool_height()
                    .and_then(|height| height.checked_add(1))
                    .and_then(|height| usize::try_from(height).ok())
                    .and_then(|height| self.state.best.get(height))
                    .and_then(|hash| self.state.known.get(hash))
                else {
                    return Vec::new();
                };
                let raw = next.raw.as_ref();
                next.block
                    .transactions()
                    .iter()
                    .enumerate()
                    .skip(1)
                    .map(|(at, tx)| {
                        let bytes = raw.and_then(|raw| raw.transactions.get(at).cloned());
                        (tx.clone(), bytes)
                    })
                    .collect()
            }
        }
    }

    /// Takes one stale-roots answer if armed.
    pub(crate) fn take_stale_root(&mut self) -> bool {
        match self.state.stale_roots {
            0 => false,
            _ => {
                self.state.stale_roots -= 1;
                true
            }
        }
    }

    /// Records a sent transaction.
    pub(crate) fn scripted_send(
        &self,
        bytes: &[u8],
    ) -> Option<Result<TransactionId, SendRawTransactionError>> {
        self.state.scripted.get(bytes).cloned()
    }

    /// Records a sent transaction.
    pub(crate) fn record_sent(&mut self, bytes: Vec<u8>) {
        self.state.sent.push(bytes);
    }
}
