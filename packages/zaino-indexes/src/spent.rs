//! I-09 `spent`: outpoint → the input that spent it.

use zaino_persistence::{BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{BlockRef, BlockTxPosition, Outpoint};

use crate::block::FoldBlock;
use crate::codec::{self, outpoint_bytes, position_bytes, OUTPOINT_BYTES, POSITION_BYTES};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "spent";

const SPENT: Namespace = Namespace::new("spent");

declared! {
    /// An outpoint as a map key: txid, then output index big-endian.
    pub const OUTPOINT: Layout = Layout::new(
        "outpoint",
        1,
        &[Field::fixed("txid", 32), Field::fixed("index", 4)],
    );
}
declared! {
    /// The spending input: height `u32`, tx index and input index `u16`
    /// (D-62), big-endian.
    pub const SPENDER: Layout = Layout::new(
        "spender",
        1,
        &[
            Field::fixed("height", 4),
            Field::fixed("tx_index", 2),
            Field::fixed("input", 2),
        ],
    );
}
declared! {
    const BY_OUTPOINT: NamespaceLayout = NamespaceLayout::map(SPENT, &OUTPOINT, &SPENDER, false);
}
const _: () = assert!(OUTPOINT.fixed_len() == OUTPOINT_BYTES);
const _: () = assert!(SPENDER.fixed_len() == POSITION_BYTES + 2);

/// The schema: one map, outpoint → spender.
pub const SCHEMA: Schema = Schema::new(NAME, 1, &[BY_OUTPOINT]);

/// The input that spent an outpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Spender {
    /// The spending transaction.
    pub position: BlockTxPosition,
    /// Its input index.
    pub input: u32,
}

/// I-09.
pub struct Spent;

/// The last stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
}

impl Index for Spent {
    const SCHEMA: Schema = SCHEMA;
    const DEPENDENCIES: &'static [&'static str] = &[crate::txids::NAME];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        Ok(backend.tip()?.map(|tip| State { tip }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let reference = block.reference();
        let mut ops = Vec::new();
        for (tx_index, tx) in (0u32..).zip(block.transactions()) {
            let position = BlockTxPosition::new(reference.height, tx_index);
            for (input, spent) in (0u32..).zip(&tx.compact().transparent_inputs) {
                let mut value = Vec::with_capacity(SPENDER.fixed_len());
                value.extend_from_slice(&position_bytes(position)?);
                value.extend_from_slice(&codec::narrow("input", input)?.to_be_bytes());
                ops.push(WriteOp::Put {
                    namespace: SPENT,
                    key: outpoint_bytes(Outpoint {
                        txid: spent.prev_txid,
                        index: spent.prev_index,
                    })
                    .to_vec(),
                    value,
                });
            }
        }
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

    /// The input that spent `outpoint`; `None` if unspent as of the tip.
    pub fn spender(&self, outpoint: Outpoint) -> Result<Option<Spender>, FoldError> {
        let key = outpoint_bytes(outpoint);
        self.backend.with_value(SPENT, &key, spender)?.transpose()
    }

    /// [`spender`](Self::spender) for each outpoint, in order, one batch read.
    pub fn spenders(&self, outpoints: &[Outpoint]) -> Result<Vec<Option<Spender>>, FoldError> {
        let keys: Vec<[u8; OUTPOINT_BYTES]> =
            outpoints.iter().copied().map(outpoint_bytes).collect();
        let keys: Vec<&[u8]> = keys.iter().map(|key| key.as_slice()).collect();
        self.backend
            .get_batch(SPENT, &keys)?
            .into_iter()
            .map(|value| value.map(|bytes| spender(&bytes)).transpose())
            .collect()
    }
}

fn spender(bytes: &[u8]) -> Result<Spender, FoldError> {
    let (position, input) =
        bytes
            .split_at_checked(POSITION_BYTES)
            .ok_or(LayoutError::FieldWidth {
                field: "spender",
                expected: SPENDER.fixed_len(),
                actual: bytes.len(),
            })?;
    Ok(Spender {
        position: codec::position(NAME, position)?,
        input: codec::array::<2>(input, "input")
            .map(|bytes| u32::from(u16::from_be_bytes(bytes)))?,
    })
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::{
        Height, PoolBalances, Script, TransactionId, TransparentInput, TransparentOutput, Zatoshis,
    };

    use super::*;
    use crate::block::FoldTx;
    use crate::testing::{backend, block_with, fold_all, tx};

    fn spending(n: u32, spends: &[(u8, u32)]) -> FoldTx {
        let mut compact = tx(n).compact().clone();
        compact.transparent_inputs = spends
            .iter()
            .map(|&(txid, index)| TransparentInput {
                prev_txid: TransactionId::from([txid; 32]),
                prev_index: index,
            })
            .collect();
        let prevouts = spends
            .iter()
            .map(|_| TransparentOutput {
                value: Zatoshis::new(1).expect("1"),
                script: Script::from(vec![]),
            })
            .collect();
        FoldTx::try_new(compact, PoolBalances::default(), prevouts).expect("paired")
    }

    fn outpoint(txid: u8, index: u32) -> Outpoint {
        Outpoint {
            txid: TransactionId::from([txid; 32]),
            index,
        }
    }

    /// Each spent outpoint names its spending input; an unspent one is absent.
    #[test]
    fn spenders_read_back_singly_and_batched() {
        let chain = [
            block_with(0, vec![tx(0)]),
            block_with(
                1,
                vec![
                    tx(1),
                    spending(2, &[(7, 0), (7, 1)]),
                    spending(3, &[(8, 4)]),
                ],
            ),
        ];
        let backend = backend(&SCHEMA);
        fold_all::<Spent>(&backend, &chain).expect("folded");
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);
        let one = Height::try_from(1).expect("1");
        let at = |tx_index, input| Spender {
            position: BlockTxPosition::new(one, tx_index),
            input,
        };
        assert_eq!(
            reader.spender(outpoint(7, 1)).expect("read"),
            Some(at(1, 1))
        );
        assert_eq!(
            reader
                .spenders(&[outpoint(7, 0), outpoint(9, 0), outpoint(8, 4)])
                .expect("batch"),
            vec![Some(at(1, 0)), None, Some(at(2, 0))]
        );
    }

    /// The schema digest every `spent_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x48, 0x37, 0x48, 0x8d, 0x2f, 0x6b, 0x30, 0xfc, 0x5b, 0x1a, 0x2f, 0x5e, 0xf8, 0xd4,
            0xf7, 0xc8, 0x64, 0x04, 0xc2, 0xbd, 0xdc, 0x88, 0x5f, 0x50, 0x06, 0xc6, 0x2f, 0x47,
            0x51, 0x6d, 0x49, 0x59,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "spent_1");
    }
}
