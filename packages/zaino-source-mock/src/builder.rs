//! Synthetic chains: deterministic, any shape a test needs, built in
//! microseconds.
//!
//! Every hash is BLAKE2b of what it names, so the same specs always give the
//! same chain, and a [`BlockSpec::seed`] tells two branches apart. Each
//! coinbase pays [`FAN`] outputs; a block's transparent spends spend its
//! parent's coinbase outputs, so spends are valid on every branch. Shielded
//! notes are counted, never decryptable: roots, tree sizes, treestates (in
//! zcashd's legacy encoding) and completed subtrees agree with the counts.

use blake2::{digest::consts::U32, Blake2b, Digest as _};
use zaino_primitives::types::{
    Block, BlockCommitments, BlockHash, BlockHeader, ChainMetadata, CompactCiphertext,
    CompactDifficulty, EphemeralKey, EquihashSolution, Height, MerkleRoot, NoteCommitment,
    Nullifier, OrchardAction, OrchardData, PoolTreestate, SaplingData, SaplingOutput, Script,
    ShieldedPool, SubtreeRoot, Transaction, TransactionId, TransparentData, TransparentInput,
    TransparentOutput, TreeRoot, TreeRootInfo, TreeRoots, TreeSize, Zatoshis,
};

use crate::block::{MockBlock, PoolTrees};
use crate::chain::{MockChain, NodeFacts};

/// Outputs each coinbase pays: the most transparent spends a block can make.
pub const FAN: u32 = 8;
/// Each coinbase output's value.
pub const COINBASE_OUTPUT: u64 = 100_000;
/// The script every coinbase output pays (P2PKH, key hash `0x11..`).
pub const MINER_SCRIPT: [u8; 25] = p2pkh([0x11; 20]);
/// The script every spend pays (P2PKH, key hash `0x22..`).
pub const RECIPIENT_SCRIPT: [u8; 25] = p2pkh([0x22; 20]);

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

/// Which pools are active from which height.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activations {
    /// Sapling from this height (`None` = never).
    pub sapling: Option<u32>,
    /// Orchard from this height.
    pub orchard: Option<u32>,
    /// Ironwood from this height.
    pub ironwood: Option<u32>,
}

impl Default for Activations {
    /// Sapling and Orchard from genesis, no Ironwood.
    fn default() -> Self {
        Self {
            sapling: Some(0),
            orchard: Some(0),
            ironwood: None,
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
    /// A regtest chain holding its genesis block.
    pub fn new() -> Self {
        Self::with(NodeFacts::regtest(), Activations::default())
    }

    /// A chain for `facts` with pools active per `activations`, holding its
    /// genesis block.
    pub fn with(facts: NodeFacts, activations: Activations) -> Self {
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
pub fn next_block(
    parent: Option<&MockBlock>,
    activations: &Activations,
    spec: BlockSpec,
) -> MockBlock {
    let height = parent.map_or(0, |parent| u32::from(parent.height()).saturating_add(1));
    let prev_hash = parent.map_or(BlockHash::from([0; 32]), MockBlock::hash);
    let parent_bytes = <[u8; 32]>::from(prev_hash);
    let tag = |what: &str, n: u64| {
        hash(&[
            what.as_bytes(),
            &parent_bytes,
            &height.to_le_bytes(),
            &spec.seed.to_le_bytes(),
            &n.to_le_bytes(),
        ])
    };

    let coinbase = Transaction {
        txid: TransactionId::from(tag("coinbase", 0)),
        transparent: TransparentData {
            inputs: Vec::new(),
            outputs: (0..FAN)
                .map(|_| TransparentOutput {
                    value: zatoshis(COINBASE_OUTPUT),
                    script: Script::new(MINER_SCRIPT.to_vec()),
                })
                .collect(),
        },
        sapling: SaplingData::default(),
        orchard: OrchardData::default(),
        ironwood: OrchardData::default(),
    };
    let mut transactions = vec![coinbase];
    let parent_coinbase = parent.and_then(|parent| parent.block.transactions().first());
    if let Some(funding) = parent_coinbase {
        for output in 0..spec.spends.min(FAN) {
            transactions.push(Transaction {
                txid: TransactionId::from(tag("spend", u64::from(output))),
                transparent: TransparentData {
                    inputs: vec![TransparentInput {
                        prev_txid: funding.txid,
                        prev_index: output,
                    }],
                    outputs: vec![TransparentOutput {
                        value: zatoshis(COINBASE_OUTPUT - 1_000),
                        script: Script::new(RECIPIENT_SCRIPT.to_vec()),
                    }],
                },
                sapling: SaplingData::default(),
                orchard: OrchardData::default(),
                ironwood: OrchardData::default(),
            });
        }
    }

    let mut shielded = Transaction {
        txid: TransactionId::from(tag("shielded", 0)),
        transparent: TransparentData::default(),
        sapling: SaplingData::default(),
        orchard: OrchardData::default(),
        ironwood: OrchardData::default(),
    };
    let note = |pool: &str, n: u32| tag(pool, u64::from(n));
    shielded.sapling.outputs = (0..spec.sapling)
        .map(|n| SaplingOutput {
            cmu: NoteCommitment::from(note("sapling cmu", n)),
            ephemeral_key: EphemeralKey::from(note("sapling epk", n)),
            enc_ciphertext: CompactCiphertext::from([0x5a; 52]),
        })
        .collect();
    let actions = |pool: &str, count: u32| -> Vec<OrchardAction> {
        (0..count)
            .map(|n| OrchardAction {
                nullifier: Nullifier::from(note(&format!("{pool} nf"), n)),
                cmx: NoteCommitment::from(note(&format!("{pool} cmx"), n)),
                ephemeral_key: EphemeralKey::from(note(&format!("{pool} epk"), n)),
                enc_ciphertext: CompactCiphertext::from([0xa5; 52]),
            })
            .collect()
    };
    shielded.orchard.actions = actions("orchard", spec.orchard);
    shielded.ironwood.actions = actions("ironwood", spec.ironwood);
    let added = [spec.sapling, spec.orchard, spec.ironwood];
    if added.iter().any(|count| *count > 0) {
        transactions.push(shielded);
    }

    let mut tree_roots = TreeRoots {
        sapling: None,
        orchard: None,
        ironwood: None,
    };
    let mut trees = PoolTrees::default();
    let mut subtrees = Vec::new();
    for (pool, count) in ShieldedPool::ALL.into_iter().zip(added) {
        let from = match pool {
            ShieldedPool::Sapling => activations.sapling,
            ShieldedPool::Orchard => activations.orchard,
            ShieldedPool::Ironwood => activations.ironwood,
        };
        if from.is_none_or(|from| height < from) {
            continue;
        }
        let before = parent
            .and_then(|parent| pool_size(&parent.tree_roots, pool))
            .unwrap_or(0);
        let size = before.saturating_add(u64::from(count));
        let root = TreeRoot::from(tag(&format!("{pool} root"), size));
        let info = TreeRootInfo {
            root,
            size: TreeSize::try_from(size).unwrap_or(TreeSize::ZERO),
        };
        let tree = PoolTreestate {
            // both adapters answer roots through `get_commitment_tree_roots`, never here
            final_root: None,
            final_state: legacy_tree(size, |n| tag(&format!("{pool} node"), n)),
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
    let header = BlockHeader {
        hash: BlockHash::from(tag("block", 0)),
        version: 4,
        prev_hash,
        height: Height::try_from(height).unwrap_or(Height::GENESIS),
        time: GENESIS_TIME.saturating_add(height.saturating_mul(75)),
        merkle_root: MerkleRoot::from(tag("merkle", 0)),
        block_commitments: BlockCommitments::from([0; 32]),
        bits: CompactDifficulty::try_from_bits(REGTEST_BITS)
            .unwrap_or_else(|_| panic!("regtest bits are valid")),
        nonce: tag("nonce", 0),
        solution: EquihashSolution::Regtest([0; 36]),
    };
    let block = Block::try_new(
        header,
        transactions,
        ChainMetadata {
            sapling_tree_size: size(ShieldedPool::Sapling),
            orchard_tree_size: size(ShieldedPool::Orchard),
            ironwood_tree_size: size(ShieldedPool::Ironwood),
        },
    )
    .unwrap_or_else(|_| panic!("a block always holds its coinbase"));
    MockBlock {
        size: estimated_size(&block),
        block,
        tree_roots,
        trees,
        subtrees,
        raw: None,
    }
}

/// zcashd's legacy `CommitmentTree` encoding of a tree of `size` leaves,
/// nodes from `node` (the empty tree for 0).
pub fn legacy_tree(size: u64, node: impl Fn(u64) -> [u8; 32]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let optional = |present: bool, at: u64, bytes: &mut Vec<u8>| match present {
        true => {
            bytes.push(1);
            bytes.extend(node(at));
        }
        false => bytes.push(0),
    };
    if size == 0 {
        return vec![0, 0, 0];
    }
    let right = size.is_multiple_of(2);
    optional(true, 0, &mut bytes);
    optional(right, 1, &mut bytes);
    // leaves held by the parents: an even count
    let parents = size - 1 - u64::from(right);
    let levels = 64 - u64::from(parents.leading_zeros());
    let count = levels.saturating_sub(1);
    bytes.push(u8::try_from(count).unwrap_or(0));
    for level in 1..=count {
        optional(parents & (1 << level) != 0, 1 + level, &mut bytes);
    }
    bytes
}

/// A synthetic block's size: its parts at their encoded widths (header with
/// a regtest solution, inputs, outputs, Sapling outputs, actions).
fn estimated_size(block: &Block) -> u64 {
    let parts: usize = block
        .transactions()
        .iter()
        .map(|tx| {
            20 + 41 * tx.transparent.inputs.len()
                + 34 * tx.transparent.outputs.len()
                + 948 * tx.sapling.outputs.len()
                + 820 * (tx.orchard.actions.len() + tx.ironwood.actions.len())
        })
        .sum();
    u64::try_from(177 + parts).unwrap_or(u64::MAX)
}

fn pool_size(roots: &TreeRoots, pool: ShieldedPool) -> Option<u64> {
    let info = match pool {
        ShieldedPool::Sapling => roots.sapling.as_ref(),
        ShieldedPool::Orchard => roots.orchard.as_ref(),
        ShieldedPool::Ironwood => roots.ironwood.as_ref(),
    }?;
    Some(u64::from(info.size.get()))
}

fn zatoshis(value: u64) -> Zatoshis {
    Zatoshis::new(value).unwrap_or_else(|_| panic!("{value} zatoshis is in range"))
}

fn hash(parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Blake2b::<U32>::new();
    for part in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}
