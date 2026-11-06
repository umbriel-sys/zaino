//! I-03 `txid_location`: txid → the (height, index) it was mined at.

use zaino_persistence::{BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{BlockRef, BlockTxPosition, TransactionId};

use crate::block::FoldBlock;
use crate::codec::{self, position_bytes, POSITION_BYTES};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "txid_location";

const LOCATIONS: Namespace = Namespace::new("txid_location");

declared! {
    /// A txid as a map key.
    pub const TXID: Layout = Layout::new("txid", 1, &[Field::fixed("txid", 32)]);
}
declared! {
    /// Where a transaction was mined: height then index, big-endian.
    pub const POSITION: Layout = Layout::new(
        "tx_position",
        1,
        &[Field::fixed("height", 4), Field::fixed("tx_index", 4)],
    );
}
declared! {
    const BY_TXID: NamespaceLayout = NamespaceLayout::map(LOCATIONS, &TXID, &POSITION, false);
}

/// The schema: one map, txid → position.
pub const SCHEMA: Schema = Schema::new(NAME, 1, &[BY_TXID]);

/// I-03.
pub struct TxidLocation;

/// The last stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
}

impl Index for TxidLocation {
    const SCHEMA: Schema = SCHEMA;
    const DEPENDENCIES: &'static [&'static str] = &[crate::txids::NAME];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        Ok(backend.tip()?.map(|tip| State { tip }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let reference = block.reference();
        let ops = (0u32..)
            .zip(block.transactions())
            .map(|(tx_index, tx)| WriteOp::Put {
                namespace: LOCATIONS,
                key: <[u8; 32]>::from(tx.compact().txid).to_vec(),
                value: position_bytes(BlockTxPosition::new(reference.height, tx_index)).to_vec(),
            })
            .collect();
        Ok(Fold {
            ops,
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

    /// Where `txid` was mined; `None` if this index never folded it.
    pub fn position(&self, txid: TransactionId) -> Result<Option<BlockTxPosition>, FoldError> {
        let Some(bytes) = self.backend.get(LOCATIONS, &<[u8; 32]>::from(txid))? else {
            return Ok(None);
        };
        codec::position(NAME, &bytes).map(Some)
    }

    /// [`position`](Self::position) for each txid, in order, one batch read.
    pub fn positions(
        &self,
        txids: &[TransactionId],
    ) -> Result<Vec<Option<BlockTxPosition>>, FoldError> {
        let keys: Vec<[u8; 32]> = txids.iter().map(|txid| <[u8; 32]>::from(*txid)).collect();
        let keys: Vec<&[u8]> = keys.iter().map(|key| key.as_slice()).collect();
        self.backend
            .get_many(LOCATIONS, &keys)?
            .into_iter()
            .map(|value| value.map(|bytes| codec::position(NAME, &bytes)).transpose())
            .collect()
    }
}

const _: () = assert!(POSITION.fixed_len() == POSITION_BYTES);

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::Height;

    use super::*;
    use crate::testing::{backend, busy, fold_all};

    /// Every folded txid reads back at its position, singly and batched;
    /// an unknown txid is absent.
    #[test]
    fn txids_read_back_at_their_positions() {
        let backend = backend(&SCHEMA);
        let chain = [busy(0, 1), busy(1, 3)];
        fold_all::<TxidLocation>(&backend, &chain).expect("two blocks");
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);

        let mut asked = Vec::new();
        let mut expected = Vec::new();
        for block in &chain {
            for (index, tx) in (0u32..).zip(block.transactions()) {
                let at = BlockTxPosition::new(block.header().height, index);
                let txid = tx.compact().txid;
                assert_eq!(reader.position(txid).expect("read"), Some(at));
                asked.push(txid);
                expected.push(Some(at));
            }
        }
        let unknown = TransactionId::from([0xee; 32]);
        asked.push(unknown);
        expected.push(None);
        assert_eq!(reader.position(unknown).expect("read"), None);
        assert_eq!(reader.positions(&asked).expect("batch"), expected);
        assert_eq!(
            reader
                .position(chain[1].transactions()[2].compact().txid)
                .expect("read"),
            Some(BlockTxPosition::new(Height::try_from(1).expect("1"), 2))
        );
    }

    /// Reopening resumes at the stored tip; a block off the tip is refused.
    #[test]
    fn resume_and_continuity() {
        let backend = backend(&SCHEMA);
        let folded = fold_all::<TxidLocation>(&backend, &[busy(0, 1)]);
        let resumed = TxidLocation::resume(&backend.reader().expect("reader")).expect("resume");
        assert_eq!(resumed, folded);
        assert!(TxidLocation::fold(resumed.as_ref(), &busy(2, 1)).is_err());
    }

    /// The schema digest every `txid_location_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x99, 0x39, 0x5b, 0xb9, 0x6b, 0xa4, 0x0a, 0x61, 0xa8, 0x96, 0xaf, 0x36, 0x82, 0x5a,
            0x1f, 0x6b, 0xd1, 0xc6, 0xef, 0x8d, 0x97, 0xdd, 0xf8, 0xec, 0xdb, 0xbd, 0x2f, 0xdf,
            0x55, 0x38, 0x60, 0x75,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "txid_location_1");
    }
}
