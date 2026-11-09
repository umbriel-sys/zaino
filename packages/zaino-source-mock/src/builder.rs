//! Synthetic chains: deterministic, any shape a test needs.
//!
//! Every block is encoded to consensus bytes in the format its height's
//! upgrade requires, and its domain [`Block`] is parsed from those bytes by
//! the raw parser at [`BlockVerification::Light`]: txids, merkle root and
//! hash are real. The mock's tests check every block shape at
//! [`BlockVerification::Full`], where zebra's parse must agree; building at
//! `Full` costs ~24x. Each coinbase pays [`FAN`]
//! outputs; a block's transparent spends spend its parent's coinbase
//! outputs, so spends are valid on every branch; a [`BlockSpec::seed`] tells
//! two branches apart. Shielded notes are never decryptable, and commitment
//! roots, tree sizes, treestates (in zcashd's legacy encoding) and completed
//! subtrees are synthetic but agree with the notes. `block_commitments` is a
//! tag.

use blake2::{digest::consts::U32, Blake2b, Digest as _};
use zaino_primitives::types::{
    ChainMetadata, Height, PoolTreestate, ShieldedPool, SubtreeRoot, TreeRoot, TreeRootInfo,
    TreeRoots, TreeSize,
};
use zaino_source_zcash_rpc::{merkle_root, parse_block, BlockVerification};

use crate::block::{MockBlock, PoolTrees, RawBlock};
use crate::chain::{MockChain, NodeFacts};
use crate::encode::{self, Action, Input, Note, Output, Tx};
use crate::schedule::Activations;

/// Outputs each coinbase pays: the most transparent spends a block can make.
pub const FAN: u32 = 8;
/// Each coinbase output's value.
pub const COINBASE_OUTPUT: u64 = 100_000;
/// The script every coinbase output pays (P2PKH, key hash `0x11..`).
pub const MINER_SCRIPT: [u8; 25] = p2pkh([0x11; 20]);
/// The script every spend pays (P2PKH, key hash `0x22..`).
pub const RECIPIENT_SCRIPT: [u8; 25] = p2pkh([0x22; 20]);
/// The most Sapling outputs one block holds.
pub const SAPLING_PER_BLOCK: u32 = 2_000;
/// The most Orchard-shaped actions one block holds.
pub const ACTIONS_PER_BLOCK: u32 = 600;

const LEAVES_PER_SUBTREE: u64 = 1 << 16;
/// Regtest's minimum difficulty.
const REGTEST_BITS: u32 = 0x200f_0f0f;
const GENESIS_TIME: u32 = 1_700_000_000;

/// A standard P2PKH script paying `hash`.
pub const fn p2pkh(hash: [u8; 20]) -> [u8; 25] {
    let mut script = [0u8; 25];
    script[0] = 0x76;
    script[1] = 0xa9;
    script[2] = 0x14;
    let mut at = 0;
    while at < 20 {
        script[3 + at] = hash[at];
        at += 1;
    }
    script[23] = 0x88;
    script[24] = 0xac;
    script
}

/// What one block holds beyond its coinbase.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockSpec {
    /// Transactions each spending one parent coinbase output (at most [`FAN`]).
    pub spends: u32,
    /// Sapling outputs added.
    pub sapling: u32,
    /// Orchard actions added.
    pub orchard: u32,
    /// Ironwood actions added.
    pub ironwood: u32,
    /// Distinguishes branches: blocks differing only in seed differ in hash.
    pub seed: u64,
}

impl BlockSpec {
    /// A block with `n` transparent spends.
    pub fn spends(n: u32) -> Self {
        Self {
            spends: n,
            ..Self::default()
        }
    }

    /// With `seed`.
    pub fn seeded(self, seed: u64) -> Self {
        Self { seed, ..self }
    }

    /// With `count` notes added to `pool`.
    pub fn notes(self, pool: ShieldedPool, count: u32) -> Self {
        match pool {
            ShieldedPool::Sapling => Self {
                sapling: count,
                ..self
            },
            ShieldedPool::Orchard => Self {
                orchard: count,
                ..self
            },
            ShieldedPool::Ironwood => Self {
                ironwood: count,
                ..self
            },
        }
    }
}

/// Builds a synthetic chain from genesis.
#[derive(Debug, Clone)]
pub struct ChainBuilder {
    facts: NodeFacts,
    activations: Activations,
    blocks: Vec<MockBlock>,
}

impl Default for ChainBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChainBuilder {
    /// A regtest chain on the default schedule, holding its genesis block.
    pub fn new() -> Self {
        Self::with(NodeFacts::regtest(), Activations::default())
    }

    /// A chain for `facts` on `activations`, which also become the facts'
    /// reported upgrades, holding its genesis block.
    pub fn with(mut facts: NodeFacts, activations: Activations) -> Self {
        facts.upgrades = activations.upgrades();
        let genesis = next_block(None, &activations, BlockSpec::default());
        Self {
            facts,
            activations,
            blocks: vec![genesis],
        }
    }

    /// Appends a block holding `spec`.
    pub fn block(mut self, spec: BlockSpec) -> Self {
        let block = next_block(self.blocks.last(), &self.activations, spec);
        self.blocks.push(block);
        self
    }

    /// Appends `count` blocks each with one transparent spend.
    pub fn blocks(self, count: u32) -> Self {
        (0..count).fold(self, |builder, _| builder.block(BlockSpec::spends(1)))
    }

    /// Appends blocks adding `total` notes to `pool`, as many per block as
    /// fit.
    pub fn notes(self, pool: ShieldedPool, total: u32) -> Self {
        let per_block = match pool {
            ShieldedPool::Sapling => SAPLING_PER_BLOCK,
            ShieldedPool::Orchard | ShieldedPool::Ironwood => ACTIONS_PER_BLOCK,
        };
        let mut builder = self;
        let mut left = total;
        while left > 0 {
            let count = left.min(per_block);
            builder = builder.block(BlockSpec::default().notes(pool, count));
            left = left.saturating_sub(count);
        }
        builder
    }

    /// The blocks so far.
    pub fn into_blocks(self) -> Vec<MockBlock> {
        self.blocks
    }

    /// The chain, every block served.
    pub fn build(self) -> MockChain {
        MockChain::new(self.facts, self.blocks)
    }
}

/// The block holding `spec` on top of `parent` (`None` = genesis).
///
/// # Panics
///
/// On a spec the chain cannot hold: notes in a pool not yet active, or more
/// than a block's bytes allow (spread them with [`ChainBuilder::notes`]).
pub fn next_block(
    parent: Option<&MockBlock>,
    activations: &Activations,
    spec: BlockSpec,
) -> MockBlock {
    let height = parent.map_or(0, |parent| u32::from(parent.height()).saturating_add(1));
    let prev_hash = parent.map_or([0; 32], |parent| <[u8; 32]>::from(parent.hash()));
    let tag = |what: &str, n: u64| {
        hash(&[
            what.as_bytes(),
            &prev_hash,
            &height.to_le_bytes(),
            &spec.seed.to_le_bytes(),
            &n.to_le_bytes(),
        ])
    };
    let added = [spec.sapling, spec.orchard, spec.ironwood];
    for (pool, count) in ShieldedPool::ALL.into_iter().zip(added) {
        if count > 0 && !activations.active(pool, height) {
            panic!("{count} {pool} notes at height {height}, before the pool activates");
        }
    }
    let commitment = |pool: ShieldedPool, n: u32| {
        encode::field(tag(&format!("{pool} commitment"), u64::from(n)))
    };

    let mut transactions = vec![Tx {
        inputs: vec![Input {
            prev_txid: [0; 32],
            prev_index: u32::MAX,
            script: encode::coinbase_script(height, spec.seed),
            sequence: u32::MAX,
        }],
        outputs: (0..FAN)
            .map(|_| Output {
                value: COINBASE_OUTPUT,
                script: MINER_SCRIPT.to_vec(),
            })
            .collect(),
        expiry: height,
        ..Tx::empty()
    }];
    if let Some(funding) = parent.and_then(|parent| parent.block.transactions().first()) {
        for output in 0..spec.spends.min(FAN) {
            transactions.push(Tx {
                inputs: vec![Input {
                    prev_txid: <[u8; 32]>::from(funding.txid),
                    prev_index: output,
                    script: P2PKH_SCRIPT_SIG.to_vec(),
                    // the seed tells a sibling branch's spend apart
                    sequence: u32::MAX.wrapping_sub(spec.seed as u32),
                }],
                outputs: vec![Output {
                    value: COINBASE_OUTPUT.saturating_sub(1_000),
                    script: RECIPIENT_SCRIPT.to_vec(),
                }],
                ..Tx::empty()
            });
        }
    }
    let actions = |pool: ShieldedPool, count: u32| -> Vec<Action> {
        (0..count)
            .map(|n| Action {
                nullifier: encode::field(tag(&format!("{pool} nullifier"), u64::from(n))),
                note: Note {
                    cm: commitment(pool, n),
                    index: n as usize,
                },
            })
            .collect()
    };
    if added.iter().any(|count| *count > 0) {
        transactions.push(Tx {
            sapling: (0..spec.sapling)
                .map(|n| Note {
                    cm: commitment(ShieldedPool::Sapling, n),
                    index: n as usize,
                })
                .collect(),
            orchard: actions(ShieldedPool::Orchard, spec.orchard),
            ironwood: actions(ShieldedPool::Ironwood, spec.ironwood),
            anchor: encode::field(tag("anchor", 0)),
            ..Tx::empty()
        });
    }

    let mut tree_roots = TreeRoots {
        sapling: None,
        orchard: None,
        ironwood: None,
    };
    let mut trees = PoolTrees::default();
    let mut subtrees = Vec::new();
    for (pool, count) in ShieldedPool::ALL.into_iter().zip(added) {
        if !activations.active(pool, height) {
            continue;
        }
        let before = parent
            .and_then(|parent| pool_size(&parent.tree_roots, pool))
            .unwrap_or(0);
        let size = before.saturating_add(u64::from(count));
        let info = TreeRootInfo {
            root: TreeRoot::from(tag(&format!("{pool} root"), size)),
            size: TreeSize::try_from(size).unwrap_or(TreeSize::ZERO),
        };
        let parent_tree = parent.and_then(|parent| match pool {
            ShieldedPool::Sapling => parent.trees.sapling.as_ref(),
            ShieldedPool::Orchard => parent.trees.orchard.as_ref(),
            ShieldedPool::Ironwood => parent.trees.ironwood.as_ref(),
        });
        let parent_leaf = parent_tree
            .and_then(|tree| last_leaf(&tree.final_state))
            .unwrap_or([0; 32]);
        let (leaf, previous) = match count {
            0 => (parent_leaf, [0; 32]),
            1 => (commitment(pool, 0), parent_leaf),
            n => (
                commitment(pool, n.saturating_sub(1)),
                commitment(pool, n.saturating_sub(2)),
            ),
        };
        let final_state = match (count, parent_tree) {
            // nothing added: the parent's tree as it was
            (0, Some(tree)) => tree.final_state.clone(),
            _ => legacy_tree(size, leaf, previous, |level, index| {
                hash(&[
                    b"node",
                    pool.to_string().as_bytes(),
                    &level.to_le_bytes(),
                    &index.to_le_bytes(),
                ])
            }),
        };
        let tree = PoolTreestate {
            // both adapters answer roots through `get_commitment_tree_roots`, never here
            final_root: None,
            final_state,
        };
        match pool {
            ShieldedPool::Sapling => (tree_roots.sapling, trees.sapling) = (Some(info), Some(tree)),
            ShieldedPool::Orchard => (tree_roots.orchard, trees.orchard) = (Some(info), Some(tree)),
            ShieldedPool::Ironwood => {
                (tree_roots.ironwood, trees.ironwood) = (Some(info), Some(tree))
            }
        }
        for index in before / LEAVES_PER_SUBTREE..size / LEAVES_PER_SUBTREE {
            let Ok(index16) = u16::try_from(index) else {
                continue;
            };
            subtrees.push((
                pool,
                index16,
                SubtreeRoot {
                    root: TreeRoot::from(tag(&format!("{pool} subtree"), index)),
                    end_height: Height::try_from(height).unwrap_or(Height::GENESIS),
                },
            ));
        }
    }
    let size = |pool| {
        pool_size(&tree_roots, pool)
            .and_then(|size| TreeSize::try_from(size).ok())
            .unwrap_or(TreeSize::ZERO)
    };
    let metadata = ChainMetadata {
        sapling_tree_size: size(ShieldedPool::Sapling),
        orchard_tree_size: size(ShieldedPool::Orchard),
        ironwood_tree_size: size(ShieldedPool::Ironwood),
    };

    let upgrade = activations.in_force(height);
    let encoded: Vec<Vec<u8>> = transactions
        .iter()
        .map(|tx| encode::transaction(tx, upgrade))
        .collect();
    let mut header = encode::Header {
        prev_hash,
        merkle_root: [0; 32],
        block_commitments: tag("block commitments", 0),
        time: GENESIS_TIME.saturating_add(height.saturating_mul(75)),
        bits: REGTEST_BITS,
        nonce: tag("nonce", 0),
    };
    let provisional = encode::block(&encode::header(&header), &encoded);
    if provisional.len() as u64 > zaino_consensus::MAX_BLOCK_BYTES {
        panic!(
            "a block of {} bytes at height {height} exceeds MAX_BLOCK_BYTES; spread notes with \
             ChainBuilder::notes",
            provisional.len()
        );
    }
    let txids = parse_block(&provisional, metadata.clone(), BlockVerification::Off)
        .unwrap_or_else(|error| panic!("the builder encoded an unparsable block: {error}"))
        .transactions()
        .iter()
        .map(|tx| <[u8; 32]>::from(tx.txid))
        .collect();
    header.merkle_root = merkle_root(txids);
    let header = encode::header(&header);
    let bytes = encode::block(&header, &encoded);
    let block = parse_block(&bytes, metadata, BlockVerification::Light)
        .unwrap_or_else(|error| panic!("the builder encoded an invalid block: {error}"));
    MockBlock {
        size: bytes.len() as u64,
        block,
        tree_roots,
        trees,
        subtrees,
        raw: Some(RawBlock {
            block: bytes,
            header,
            transactions: encoded,
        }),
    }
}

/// A P2PKH spend's shape: a 71-byte signature push, a 33-byte key push.
const P2PKH_SCRIPT_SIG: [u8; 106] = {
    let mut script = [0x30_u8; 106];
    script[0] = 71;
    script[72] = 33;
    script[73] = 0x02;
    script
};

impl Tx {
    fn empty() -> Self {
        Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
            expiry: 0,
            sapling: Vec::new(),
            orchard: Vec::new(),
            ironwood: Vec::new(),
            anchor: [0; 32],
        }
    }
}

/// zcashd's legacy `CommitmentTree` encoding of a tree of `size` leaves (the
/// empty tree for 0): `leaf` is the last note, `previous` the one before it
/// (the left leaf at an even size), and `node(level, index)` each complete
/// left subtree the frontier holds above the leaves.
pub fn legacy_tree(
    size: u64,
    leaf: [u8; 32],
    previous: [u8; 32],
    node: impl Fn(u32, u64) -> [u8; 32],
) -> Vec<u8> {
    let optional = |value: Option<[u8; 32]>, bytes: &mut Vec<u8>| match value {
        Some(value) => {
            bytes.push(1);
            bytes.extend(value);
        }
        None => bytes.push(0),
    };
    let Some(position) = size.checked_sub(1) else {
        return vec![0, 0, 0];
    };
    let mut bytes = Vec::new();
    match position % 2 {
        0 => {
            optional(Some(leaf), &mut bytes);
            optional(None, &mut bytes);
        }
        _ => {
            optional(Some(previous), &mut bytes);
            optional(Some(leaf), &mut bytes);
        }
    }
    // parent at level L iff bit L of the position: the left sibling subtree
    let levels = (64 - position.leading_zeros()).saturating_sub(1);
    bytes.push(u8::try_from(levels).unwrap_or(0));
    for level in 1..=levels {
        let present = position & (1 << level) != 0;
        optional(
            present.then(|| node(level, (position >> level) ^ 1)),
            &mut bytes,
        );
    }
    bytes
}

/// The last leaf a legacy-encoded tree holds.
fn last_leaf(bytes: &[u8]) -> Option<[u8; 32]> {
    let left: [u8; 32] = bytes.get(1..33)?.try_into().ok()?;
    match bytes.get(33) {
        Some(1) => bytes.get(34..66)?.try_into().ok(),
        _ => Some(left),
    }
}

fn pool_size(roots: &TreeRoots, pool: ShieldedPool) -> Option<u64> {
    let info = match pool {
        ShieldedPool::Sapling => roots.sapling.as_ref(),
        ShieldedPool::Orchard => roots.orchard.as_ref(),
        ShieldedPool::Ironwood => roots.ironwood.as_ref(),
    }?;
    Some(u64::from(info.size.get()))
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}
