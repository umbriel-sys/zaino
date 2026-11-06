//! Fixtures the index tests fold: a small consistent chain and a backend.

use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendWriter as _, WriteOp};
use zaino_primitives::types::{
    BlockCommitments, BlockHash, BlockHeader, BlockRef, CompactDifficulty, EquihashSolution,
    Height, MerkleRoot, PoolBalances, PreIndexCompactTx, TransactionId, TreeRoots,
};

use crate::block::{FoldBlock, FoldTx};
use crate::index::{Fold, Index};
use crate::schema::Schema;

/// Regtest's easiest target, a valid compact encoding.
pub(crate) const REGTEST_BITS: u32 = 0x207f_ffff;

/// The hash of block `n` in the fixture chain.
pub(crate) fn hash(n: u32) -> BlockHash {
    let mut bytes = [0xab; 32];
    bytes[..4].copy_from_slice(&n.to_be_bytes());
    BlockHash::from(bytes)
}

/// The header of block `n`, which extends block `n - 1`.
pub(crate) fn header(n: u32) -> BlockHeader {
    BlockHeader {
        hash: hash(n),
        version: 4,
        prev_hash: match n {
            0 => BlockHash::from([0; 32]),
            n => hash(n - 1),
        },
        height: Height::try_from(n).expect("fixture height"),
        time: 1_700_000_000 + n,
        merkle_root: MerkleRoot::from([n as u8; 32]),
        block_commitments: BlockCommitments::from([0x11; 32]),
        bits: CompactDifficulty::try_from_bits(REGTEST_BITS).expect("regtest bits"),
        nonce: [n as u8 ^ 0x55; 32],
        solution: EquihashSolution::Regtest([n as u8; 36]),
    }
}

/// A transaction with no inputs, outputs or pools; txid from `n`.
pub(crate) fn tx(n: u32) -> FoldTx {
    let mut txid = [0xcd; 32];
    txid[..4].copy_from_slice(&n.to_be_bytes());
    let compact = PreIndexCompactTx {
        txid: TransactionId::from(txid),
        transparent_inputs: Vec::new(),
        transparent_outputs: Vec::new(),
        sapling_nullifiers: Vec::new(),
        sapling_outputs: Vec::new(),
        orchard_actions: Vec::new(),
        ironwood_actions: Vec::new(),
    };
    FoldTx::try_new(compact, PoolBalances::default(), Vec::new()).expect("no inputs")
}

/// A block of `header` holding only a coinbase.
pub(crate) fn block_of(header: BlockHeader) -> FoldBlock {
    let roots = TreeRoots {
        sapling: None,
        orchard: None,
        ironwood: None,
    };
    let coinbase = tx(u32::from(header.height));
    FoldBlock::try_new(header, vec![coinbase], roots).expect("fixture block")
}

/// Block `n` of the fixture chain.
pub(crate) fn block(n: u32) -> FoldBlock {
    block_of(header(n))
}

/// An empty backend declaring `schema`'s namespaces.
pub(crate) fn backend(schema: &Schema) -> InMemoryBackend {
    InMemoryBackend::new(&schema.specs()).expect("fixture specs")
}

/// Folds `blocks` into `backend` one commit each, as a writer would, and
/// returns the state after the last.
pub(crate) fn fold_all<I: Index>(
    backend: &InMemoryBackend,
    blocks: &[FoldBlock],
) -> Option<I::State> {
    let mut state = I::resume(&backend.reader().expect("reader")).expect("resume");
    for block in blocks {
        let Fold { ops, next } = I::fold(state.as_ref(), block).expect("fold");
        commit(backend, block.reference(), ops);
        state = Some(next);
    }
    state
}

/// One commit at `tip`.
pub(crate) fn commit(backend: &InMemoryBackend, tip: BlockRef, ops: Vec<WriteOp>) {
    backend
        .writer()
        .expect("writer")
        .commit(tip, ops)
        .expect("commit");
}
