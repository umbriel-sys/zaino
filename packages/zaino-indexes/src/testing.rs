//! Fixtures the index tests fold: a small consistent chain and a backend.

use zaino_chain_store::FrozenBlock;
use zaino_persistence::in_memory::InMemoryBackend;
use zaino_persistence::{Backend, BackendWriter as _, WriteOp};
use zaino_primitives::types::{
    BlockCommitments, BlockHash, BlockHeader, BlockRef, CompactDifficulty, EquihashSolution,
    Height, MerkleRoot, TreeRoots,
};

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

/// Block `n` of a chain whose block `n` extends block `n - 1`.
pub(crate) fn block(n: u32) -> FrozenBlock {
    FrozenBlock {
        header: BlockHeader {
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
        },
        transactions: Vec::new(),
        tree_roots: TreeRoots {
            sapling: None,
            orchard: None,
            ironwood: None,
        },
    }
}

/// An empty backend declaring `schema`'s namespaces.
pub(crate) fn backend(schema: &Schema) -> InMemoryBackend {
    InMemoryBackend::new(&schema.specs()).expect("fixture specs")
}

/// Folds `blocks` into `backend` one commit each, as a writer would, and
/// returns the state after the last.
pub(crate) fn fold_all<I>(backend: &InMemoryBackend, blocks: &[FrozenBlock]) -> Option<I::State>
where
    I: Index<Deps<zaino_persistence::in_memory::InMemoryReader> = ()>,
{
    let mut state = I::resume(&backend.reader().expect("reader")).expect("resume");
    for block in blocks {
        let Fold { ops, next } =
            I::fold::<zaino_persistence::in_memory::InMemoryReader>(state.as_ref(), block, &())
                .expect("fold");
        commit(backend, &block.header, ops);
        state = Some(next);
    }
    state
}

/// One commit at `header`'s block.
pub(crate) fn commit(backend: &InMemoryBackend, header: &BlockHeader, ops: Vec<WriteOp>) {
    let tip = BlockRef {
        hash: header.hash,
        height: header.height,
    };
    backend
        .writer()
        .expect("writer")
        .commit(tip, ops)
        .expect("commit");
}
