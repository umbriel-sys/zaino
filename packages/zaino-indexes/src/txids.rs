//! I-02 `txids`: height → the block's txids, in block order.
//!
//! The one location → txid table: I-03, I-05, I-09 and I-10 name a
//! transaction by (height, index) and resolve its txid here, one positional
//! read.

use core::ops::Range;

use zaino_persistence::{sequence_key, BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{Height, TransactionId};

use crate::block::FoldBlock;
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "txids";

const TXIDS: Namespace = Namespace::new("txids");
const TXID_BYTES: usize = 32;

declared! {
    /// A block's transaction count, then its txids at a 32-byte stride.
    pub const BLOCK_TXIDS: Layout = Layout::new(
        "block_txids",
        1,
        &[Field::fixed("count", 4), Field::var("txids")],
    );
}
const COUNT: usize = 0;
const LIST: usize = 1;
declared! { const COUNT_RANGE: Range<usize> = BLOCK_TXIDS.fixed_range(COUNT); }

/// The schema: one record per height.
pub const SCHEMA: Schema = Schema::new(NAME, 1, &[NamespaceLayout::sequence(TXIDS, &BLOCK_TXIDS)]);

/// I-02.
pub struct Txids;

/// The last stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: zaino_primitives::types::BlockRef,
}

impl Index for Txids {
    const SCHEMA: Schema = SCHEMA;
    const DEPENDENCIES: &'static [&'static str] = &[];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        if Reader::new(backend).tx_count(tip.height)?.is_none() {
            return Err(FoldError::continuity(NAME, "tip without its txids"));
        }
        Ok(Some(State { tip }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let transactions = block.transactions();
        let count = u32::try_from(transactions.len()).map_err(|_| LayoutError::Overflow)?;
        let list = transactions
            .iter()
            .flat_map(|tx| <[u8; TXID_BYTES]>::from(tx.compact().txid))
            .collect::<Vec<u8>>();
        let record = BLOCK_TXIDS.encode(&[&count.to_le_bytes(), &list])?;
        let reference = block.reference();
        Ok(Fold {
            ops: vec![WriteOp::Put {
                namespace: TXIDS,
                key: sequence_key(u64::from(reference.height)),
                value: record,
            }],
            next: State { tip: reference },
        })
    }
}

/// Typed reads over a backend holding this index.
#[derive(Debug, Clone)]
pub struct Reader<'a, R> {
    backend: &'a R,
}

impl<'a, R: BackendReader> Reader<'a, R> {
    /// Reads through `backend`.
    pub fn new(backend: &'a R) -> Self {
        Self { backend }
    }

    /// Every txid at `height`, in block order.
    pub fn txids(&self, height: Height) -> Result<Option<Vec<TransactionId>>, FoldError> {
        let Some(record) = self.backend.get(TXIDS, &key(height))? else {
            return Ok(None);
        };
        let count = decode_count(BLOCK_TXIDS.field(&record, COUNT)?)?;
        let list = BLOCK_TXIDS.field(&record, LIST)?;
        let chunks = list.chunks_exact(TXID_BYTES);
        let expected = usize::try_from(count).map_err(|_| LayoutError::Overflow)?;
        if !chunks.remainder().is_empty() || chunks.len() != expected {
            return Err(FoldError::continuity(
                NAME,
                format!("{} txid bytes for {count} at height {height}", list.len()),
            ));
        }
        chunks
            .map(|bytes| txid(bytes).map_err(FoldError::from))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
    }

    /// How many transactions the block at `height` holds.
    pub fn tx_count(&self, height: Height) -> Result<Option<u32>, FoldError> {
        let Some(bytes) = self.backend.get_range(TXIDS, &key(height), COUNT_RANGE)? else {
            return Ok(None);
        };
        decode_count(&bytes).map(Some)
    }

    /// The txid at (`height`, `index`): two field reads, never the record.
    pub fn txid(&self, height: Height, index: u32) -> Result<Option<TransactionId>, FoldError> {
        match self.tx_count(height)? {
            Some(count) if index < count => {}
            _ => return Ok(None),
        }
        let index = usize::try_from(index).map_err(|_| LayoutError::Overflow)?;
        let range = BLOCK_TXIDS.element_range(LIST, index, TXID_BYTES)?;
        let Some(bytes) = self.backend.get_range(TXIDS, &key(height), range)? else {
            return Ok(None);
        };
        Ok(Some(txid(&bytes)?))
    }
}

fn key(height: Height) -> Vec<u8> {
    sequence_key(u64::from(height))
}

fn decode_count(bytes: &[u8]) -> Result<u32, FoldError> {
    let bytes = <[u8; 4]>::try_from(bytes).map_err(|_| LayoutError::FieldWidth {
        field: "count",
        expected: 4,
        actual: bytes.len(),
    })?;
    Ok(u32::from_le_bytes(bytes))
}

fn txid(bytes: &[u8]) -> Result<TransactionId, LayoutError> {
    <[u8; TXID_BYTES]>::try_from(bytes)
        .map(TransactionId::from)
        .map_err(|_| LayoutError::FieldWidth {
            field: "txid",
            expected: TXID_BYTES,
            actual: bytes.len(),
        })
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};

    use super::*;
    use crate::block::FoldBlock;
    use crate::testing::{backend, block, fold_all, header, tx};

    /// Block `n` with `count` transactions, txids `n * 100 + i`.
    fn busy(n: u32, count: u32) -> FoldBlock {
        let txs = (0..count).map(|i| tx(n * 100 + i)).collect();
        FoldBlock::try_new(header(n), txs, block(n).tree_roots().clone()).expect("fixture")
    }

    fn txids_of(block: &FoldBlock) -> Vec<TransactionId> {
        block
            .transactions()
            .iter()
            .map(|tx| tx.compact().txid)
            .collect()
    }

    /// Every block's txids read back whole, by count and by position; past
    /// the count and past the tip is absent.
    #[test]
    fn txids_read_back_whole_and_by_position() {
        let backend = backend(&SCHEMA);
        let chain = [busy(0, 1), busy(1, 3), busy(2, 2)];
        let state = fold_all::<Txids>(&backend, &chain).expect("three blocks");
        assert_eq!(state.tip, chain[2].reference());
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);

        for block in &chain {
            let height = block.header().height;
            let expected = txids_of(block);
            assert_eq!(reader.txids(height).expect("txids"), Some(expected.clone()));
            let count = u32::try_from(expected.len()).expect("small");
            assert_eq!(reader.tx_count(height).expect("count"), Some(count));
            for (index, txid) in (0..).zip(&expected) {
                assert_eq!(reader.txid(height, index).expect("txid"), Some(*txid));
            }
            assert_eq!(reader.txid(height, count).expect("past"), None);
        }
        let above = Height::try_from(3).expect("3");
        assert_eq!(reader.txids(above).expect("absent"), None);
        assert_eq!(reader.txid(above, 0).expect("absent"), None);
    }

    /// Reopening resumes at the stored tip; a block off the tip is refused.
    #[test]
    fn resume_and_continuity() {
        let backend = backend(&SCHEMA);
        assert_eq!(
            Txids::resume(&backend.reader().expect("reader")).expect("empty"),
            None
        );
        let folded = fold_all::<Txids>(&backend, &[busy(0, 1), busy(1, 2)]);
        let resumed = Txids::resume(&backend.reader().expect("reader")).expect("resume");
        assert_eq!(resumed, folded);
        let gap = Txids::fold(resumed.as_ref(), &busy(3, 1));
        assert!(
            matches!(&gap, Err(FoldError::Continuity { index: NAME, .. })),
            "{gap:?}"
        );
    }

    /// The schema digest every `txids_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x41, 0x0f, 0xbc, 0xea, 0xab, 0xba, 0x95, 0x75, 0x28, 0x04, 0x8c, 0x5f, 0x86, 0xdd,
            0xe2, 0xa8, 0xb3, 0x62, 0x38, 0x02, 0x43, 0x34, 0x10, 0xf1, 0xba, 0x5f, 0x44, 0x73,
            0x76, 0x28, 0x71, 0x41,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "txids_1");
    }
}
