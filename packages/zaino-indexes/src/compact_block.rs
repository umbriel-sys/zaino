//! I-05 `compact_block`: the shielded axes of every compact block (§9.6 (e)).
//!
//! One fixed-width element sequence per request axis, so a pool filter reads
//! exactly the bytes it returns:
//! - `sapling_nf` (32 B), `sapling_out` (cmu, epk, ciphertext: 116 B),
//! - `orchard_nf` and `orchard_out`, `ironwood_nf` and `ironwood_out`
//!   (one element per action in each).
//!
//! `layout`, one record per height, holds where the height starts in each
//! column (`u32`: no pool's tree holds more than 2³² notes, and a nullifier
//! spends one), then per transaction its four element counts (`u16`) and fee
//! (`u64`, D-60). Txids come from I-02, headers from I-01, the transparent
//! axis from I-04.

use core::num::NonZeroUsize;
use core::ops::Bound;

use zaino_persistence::{
    sequence_key, BackendReader, Namespace, ScanDirection, ScanRange, WriteOp,
};
use zaino_primitives::types::{
    BlockRef, CompactCiphertext, EphemeralKey, Height, NoteCommitment, Nullifier, OrchardAction,
    SaplingOutput, ShieldedPool, SignedZatoshis, Zatoshis,
};

use crate::block::{FoldBlock, FoldTx};
use crate::codec::{self, array, elements, le_u32, le_u64};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "compact_block";

const LAYOUT_NS: Namespace = Namespace::new("layout");
const SAPLING_NF: Namespace = Namespace::new("sapling_nf");
const SAPLING_OUT: Namespace = Namespace::new("sapling_out");
const ORCHARD_NF: Namespace = Namespace::new("orchard_nf");
const ORCHARD_OUT: Namespace = Namespace::new("orchard_out");
const IRONWOOD_NF: Namespace = Namespace::new("ironwood_nf");
const IRONWOOD_OUT: Namespace = Namespace::new("ironwood_out");

const NULLIFIER_BYTES: usize = 32;
const OUTPUT_BYTES: usize = 32 + 32 + CompactCiphertext::LENGTH;
/// Per transaction: four `u16` counts, then the `u64` fee.
const TX_BYTES: usize = 16;
/// Bytes a range scan visits per chunk.
const CHUNK: NonZeroUsize = match NonZeroUsize::new(64 * 1024) {
    Some(chunk) => chunk,
    None => NonZeroUsize::MIN,
};

declared! {
    /// Per height: column starts, transaction count, then per-tx layouts.
    pub const BLOCK_LAYOUT: Layout = Layout::new(
        "compact_layout",
        1,
        &[
            Field::fixed("sapling_nf_start", 4),
            Field::fixed("sapling_out_start", 4),
            Field::fixed("orchard_start", 4),
            Field::fixed("ironwood_start", 4),
            Field::fixed("count", 4),
            Field::var("txs"),
        ],
    );
}
declared! {
    /// A nullifier.
    pub const NULLIFIER: Layout = Layout::new("nullifier", 1, &[Field::fixed("nullifier", 32)]);
}
declared! {
    /// A note's compact output: commitment, ephemeral key, ciphertext head.
    pub const OUTPUT: Layout = Layout::new(
        "compact_output",
        1,
        &[
            Field::fixed("commitment", 32),
            Field::fixed("ephemeral_key", 32),
            Field::fixed("ciphertext", CompactCiphertext::LENGTH),
        ],
    );
}
const _: () = assert!(OUTPUT.fixed_len() == OUTPUT_BYTES);
const _: () = assert!(NULLIFIER.fixed_len() == NULLIFIER_BYTES);
const COUNT: usize = 4;
const TXS: usize = 5;

/// The schema: `layout` and the six element columns.
pub const SCHEMA: Schema = Schema::new(
    NAME,
    1,
    &[
        NamespaceLayout::sequence(LAYOUT_NS, &BLOCK_LAYOUT),
        NamespaceLayout::sequence(SAPLING_NF, &NULLIFIER),
        NamespaceLayout::sequence(SAPLING_OUT, &OUTPUT),
        NamespaceLayout::sequence(ORCHARD_NF, &NULLIFIER),
        NamespaceLayout::sequence(ORCHARD_OUT, &OUTPUT),
        NamespaceLayout::sequence(IRONWOOD_NF, &NULLIFIER),
        NamespaceLayout::sequence(IRONWOOD_OUT, &OUTPUT),
    ],
);

/// Where a height starts in each column; also the next free positions.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ColumnStarts {
    /// `sapling_nf`.
    pub sapling_spends: u32,
    /// `sapling_out`.
    pub sapling_outputs: u32,
    /// `orchard_nf` and `orchard_out`.
    pub orchard_actions: u32,
    /// `ironwood_nf` and `ironwood_out`.
    pub ironwood_actions: u32,
}

/// One transaction's share of each column, and its fee.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxLayout {
    /// Sapling spends.
    pub sapling_spends: u16,
    /// Sapling outputs.
    pub sapling_outputs: u16,
    /// Orchard actions.
    pub orchard_actions: u16,
    /// Ironwood actions.
    pub ironwood_actions: u16,
    /// The fee; zero for the coinbase.
    pub fee: Zatoshis,
}

/// A height's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockLayout {
    /// Where the height starts in each column.
    pub starts: ColumnStarts,
    /// Per transaction, in block order.
    pub txs: Vec<TxLayout>,
}

/// I-05.
pub struct CompactBlock;

/// The last stored block and the next free position in each column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
    /// The next free positions.
    pub next: ColumnStarts,
}

impl Index for CompactBlock {
    const SCHEMA: Schema = SCHEMA;
    const READS: &'static [&'static str] = &[
        crate::headers::NAME,
        crate::txids::NAME,
        crate::transparent::NAME,
        crate::chain_metadata::NAME,
    ];
    const BUILDS_FROM: &'static [&'static str] =
        &[crate::txid_location::NAME, crate::transparent::NAME];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        let layout = Reader::new(backend)
            .layout(tip.height)?
            .ok_or_else(|| FoldError::continuity(NAME, "tip without its layout"))?;
        let mut next = layout.starts;
        for tx in &layout.txs {
            next = advance(next, tx)?;
        }
        Ok(Some(State { tip, next }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let starts = state.map(|state| state.next).unwrap_or_default();
        let mut next = starts;
        let mut txs = Vec::with_capacity(block.transactions().len().saturating_mul(TX_BYTES));
        let mut ops = Vec::new();
        for (tx_index, tx) in block.transactions().iter().enumerate() {
            let compact = tx.compact();
            let layout = TxLayout {
                sapling_spends: narrow_len("sapling_spends", compact.sapling_nullifiers.len())?,
                sapling_outputs: narrow_len("sapling_outputs", compact.sapling_outputs.len())?,
                orchard_actions: narrow_len("orchard_actions", compact.orchard_actions.len())?,
                ironwood_actions: narrow_len("ironwood_actions", compact.ironwood_actions.len())?,
                fee: match tx_index {
                    0 => Zatoshis::ZERO,
                    _ => fee(tx)?,
                },
            };
            let mut at = next;
            for nullifier in &compact.sapling_nullifiers {
                ops.push(element(
                    SAPLING_NF,
                    &mut at.sapling_spends,
                    <[u8; 32]>::from(*nullifier).to_vec(),
                )?);
            }
            for output in &compact.sapling_outputs {
                ops.push(element(
                    SAPLING_OUT,
                    &mut at.sapling_outputs,
                    output_bytes(output.cmu, output.ephemeral_key, output.enc_ciphertext),
                )?);
            }
            for (pool, actions, position) in [
                (
                    ShieldedPool::Orchard,
                    &compact.orchard_actions,
                    &mut at.orchard_actions,
                ),
                (
                    ShieldedPool::Ironwood,
                    &compact.ironwood_actions,
                    &mut at.ironwood_actions,
                ),
            ] {
                let (nf, out) = pool_columns(pool);
                for action in actions {
                    let mut out_at = *position;
                    ops.push(element(
                        out,
                        &mut out_at,
                        output_bytes(action.cmx, action.ephemeral_key, action.enc_ciphertext),
                    )?);
                    ops.push(element(
                        nf,
                        position,
                        <[u8; 32]>::from(action.nullifier).to_vec(),
                    )?);
                }
            }
            next = at;
            txs.extend_from_slice(&tx_bytes(&layout));
        }
        let reference = block.reference();
        let record = BLOCK_LAYOUT.encode(&[
            &starts.sapling_spends.to_le_bytes(),
            &starts.sapling_outputs.to_le_bytes(),
            &starts.orchard_actions.to_le_bytes(),
            &starts.ironwood_actions.to_le_bytes(),
            &codec::count(block.transactions().len())?.to_le_bytes(),
            &txs,
        ])?;
        ops.push(WriteOp::Put {
            namespace: LAYOUT_NS,
            key: sequence_key(u64::from(reference.height)),
            value: record,
        });
        Ok(Fold {
            ops,
            next: State {
                tip: reference,
                next,
            },
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

    /// The layout at `height`.
    pub fn layout(&self, height: Height) -> Result<Option<BlockLayout>, FoldError> {
        self.backend
            .with_value(LAYOUT_NS, &sequence_key(u64::from(height)), block_layout)?
            .transpose()
    }

    /// `count` Sapling nullifiers from position `start`.
    pub fn sapling_nullifiers(&self, start: u32, count: u32) -> Result<Vec<Nullifier>, FoldError> {
        self.column(SAPLING_NF, start, count, |bytes| {
            Ok(Nullifier::from(array::<32>(bytes, "nullifier")?))
        })
    }

    /// `count` Sapling outputs from position `start`.
    pub fn sapling_outputs(&self, start: u32, count: u32) -> Result<Vec<SaplingOutput>, FoldError> {
        self.column(SAPLING_OUT, start, count, |bytes| {
            let (cmu, ephemeral_key, enc_ciphertext) = output_parts(bytes)?;
            Ok(SaplingOutput {
                cmu,
                ephemeral_key,
                enc_ciphertext,
            })
        })
    }

    /// `count` actions of `pool` (Orchard or Ironwood) from position `start`.
    pub fn actions(
        &self,
        pool: ShieldedPool,
        start: u32,
        count: u32,
    ) -> Result<Vec<OrchardAction>, FoldError> {
        if pool == ShieldedPool::Sapling {
            return Err(FoldError::continuity(
                NAME,
                "sapling has outputs and spends, not actions",
            ));
        }
        let (nf, out) = pool_columns(pool);
        let nullifiers = self.column(nf, start, count, |bytes| {
            Ok(Nullifier::from(array::<32>(bytes, "nullifier")?))
        })?;
        let outputs = self.column(out, start, count, output_parts)?;
        Ok(nullifiers
            .into_iter()
            .zip(outputs)
            .map(
                |(nullifier, (cmx, ephemeral_key, enc_ciphertext))| OrchardAction {
                    nullifier,
                    cmx,
                    ephemeral_key,
                    enc_ciphertext,
                },
            )
            .collect())
    }

    /// `count` elements of `namespace` from `start`, all present or an error.
    fn column<T: Send + 'static>(
        &self,
        namespace: Namespace,
        start: u32,
        count: u32,
        decode: fn(&[u8]) -> Result<T, FoldError>,
    ) -> Result<Vec<T>, FoldError> {
        if count == 0 {
            return Ok(Vec::new());
        }
        let end = start.checked_add(count).ok_or(LayoutError::Overflow)?;
        let range = ScanRange {
            lower: Bound::Included(sequence_key(u64::from(start))),
            upper: Bound::Excluded(sequence_key(u64::from(end))),
            direction: ScanDirection::Forward,
            chunk_bytes: CHUNK,
        };
        let mut items = Vec::with_capacity(usize::try_from(count).unwrap_or_default());
        for chunk in self
            .backend
            .scan_range(namespace, range, move |_, value| Some(decode(value)))
        {
            for item in chunk? {
                items.push(item?);
            }
        }
        match codec::count(items.len())? == count {
            true => Ok(items),
            false => Err(FoldError::continuity(
                NAME,
                format!("{} of {count} elements at {start}", items.len()),
            )),
        }
    }
}

fn pool_columns(pool: ShieldedPool) -> (Namespace, Namespace) {
    match pool {
        ShieldedPool::Ironwood => (IRONWOOD_NF, IRONWOOD_OUT),
        ShieldedPool::Orchard | ShieldedPool::Sapling => (ORCHARD_NF, ORCHARD_OUT),
    }
}

fn narrow_len(field: &'static str, len: usize) -> Result<u16, LayoutError> {
    codec::narrow(field, codec::count(len)?)
}

/// The column starts after `tx`.
fn advance(starts: ColumnStarts, tx: &TxLayout) -> Result<ColumnStarts, LayoutError> {
    let add = |start: u32, count: u16| {
        start
            .checked_add(u32::from(count))
            .ok_or(LayoutError::Overflow)
    };
    Ok(ColumnStarts {
        sapling_spends: add(starts.sapling_spends, tx.sapling_spends)?,
        sapling_outputs: add(starts.sapling_outputs, tx.sapling_outputs)?,
        orchard_actions: add(starts.orchard_actions, tx.orchard_actions)?,
        ironwood_actions: add(starts.ironwood_actions, tx.ironwood_actions)?,
    })
}

/// One element appended at `position`, which then moves on.
fn element(
    namespace: Namespace,
    position: &mut u32,
    value: Vec<u8>,
) -> Result<WriteOp, LayoutError> {
    let key = sequence_key(u64::from(*position));
    *position = position.checked_add(1).ok_or(LayoutError::Overflow)?;
    Ok(WriteOp::Put {
        namespace,
        key,
        value,
    })
}

fn output_bytes(
    commitment: NoteCommitment,
    ephemeral_key: EphemeralKey,
    ciphertext: CompactCiphertext,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(OUTPUT_BYTES);
    out.extend_from_slice(&<[u8; 32]>::from(commitment));
    out.extend_from_slice(&<[u8; 32]>::from(ephemeral_key));
    out.extend_from_slice(&<[u8; CompactCiphertext::LENGTH]>::from(ciphertext));
    out
}

fn output_parts(
    bytes: &[u8],
) -> Result<(NoteCommitment, EphemeralKey, CompactCiphertext), FoldError> {
    let field = |index| OUTPUT.field(bytes, index);
    Ok((
        NoteCommitment::from(array::<32>(field(0)?, "commitment")?),
        EphemeralKey::from(array::<32>(field(1)?, "ephemeral_key")?),
        CompactCiphertext::from(array::<{ CompactCiphertext::LENGTH }>(
            field(2)?,
            "ciphertext",
        )?),
    ))
}

fn tx_bytes(tx: &TxLayout) -> [u8; TX_BYTES] {
    let mut out = [0; TX_BYTES];
    let counts = [
        tx.sapling_spends,
        tx.sapling_outputs,
        tx.orchard_actions,
        tx.ironwood_actions,
    ];
    let (head, fee) = out.split_at_mut(8);
    for (slot, count) in head.chunks_exact_mut(2).zip(counts) {
        slot.copy_from_slice(&count.to_le_bytes());
    }
    fee.copy_from_slice(&u64::from(tx.fee).to_le_bytes());
    out
}

/// The layout a stored record holds.
fn block_layout(record: &[u8]) -> Result<BlockLayout, FoldError> {
    let start = |field: usize| -> Result<u32, FoldError> {
        Ok(le_u32(BLOCK_LAYOUT.field(record, field)?, "start")?)
    };
    let starts = ColumnStarts {
        sapling_spends: start(0)?,
        sapling_outputs: start(1)?,
        orchard_actions: start(2)?,
        ironwood_actions: start(3)?,
    };
    let count = le_u32(BLOCK_LAYOUT.field(record, COUNT)?, "count")?;
    let txs = elements(NAME, count, BLOCK_LAYOUT.field(record, TXS)?, TX_BYTES)?
        .map(tx_layout)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(BlockLayout { starts, txs })
}

fn tx_layout(bytes: &[u8]) -> Result<TxLayout, FoldError> {
    let bytes = array::<TX_BYTES>(bytes, "tx_layout")?;
    let (head, fee) = bytes.split_at(8);
    let mut counts = head
        .chunks_exact(2)
        .map(|count| array::<2>(count, "count").map(u16::from_le_bytes));
    let mut count = || {
        counts.next().unwrap_or(Err(LayoutError::Truncated {
            field: "count",
            needed: 8,
            len: 0,
        }))
    };
    let layout = TxLayout {
        sapling_spends: count()?,
        sapling_outputs: count()?,
        orchard_actions: count()?,
        ironwood_actions: count()?,
        fee: {
            let fee = le_u64(fee, "fee")?;
            Zatoshis::new(fee)
                .map_err(|_| FoldError::continuity(NAME, format!("stored fee {fee}")))?
        },
    };
    Ok(layout)
}

/// A transaction's fee: transparent in, less transparent out, plus every
/// shielded pool's balance. A negative fee is refused.
fn fee(tx: &FoldTx) -> Result<Zatoshis, FoldError> {
    let sum = |values: &[zaino_primitives::types::TransparentOutput]| -> Option<i128> {
        values.iter().try_fold(0i128, |total, output| {
            total.checked_add(i128::from(u64::from(output.value)))
        })
    };
    let balances = tx.balances();
    let shielded = [balances.sapling, balances.orchard, balances.ironwood]
        .into_iter()
        .flatten()
        .try_fold(0i128, |total, balance: SignedZatoshis| {
            total.checked_add(i128::from(balance.as_i64()))
        });
    let total = sum(tx.prevouts())
        .zip(sum(&tx.compact().transparent_outputs))
        .and_then(|(inputs, outputs)| inputs.checked_sub(outputs))
        .zip(shielded)
        .and_then(|(transparent, shielded)| transparent.checked_add(shielded))
        .ok_or(LayoutError::Overflow)?;
    u64::try_from(total)
        .ok()
        .and_then(|fee| Zatoshis::new(fee).ok())
        .ok_or_else(|| FoldError::continuity(NAME, format!("fee {total} of {}", tx.compact().txid)))
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::{
        PoolBalances, Script, TransactionId, TransparentInput, TransparentOutput,
    };

    use super::*;
    use crate::testing::{backend, block_with, fold_all, tx};

    fn output(byte: u8) -> SaplingOutput {
        SaplingOutput {
            cmu: NoteCommitment::from([byte; 32]),
            ephemeral_key: EphemeralKey::from([byte ^ 1; 32]),
            enc_ciphertext: CompactCiphertext::from([byte ^ 2; CompactCiphertext::LENGTH]),
        }
    }

    fn action(byte: u8) -> OrchardAction {
        OrchardAction {
            nullifier: Nullifier::from([byte; 32]),
            cmx: NoteCommitment::from([byte ^ 3; 32]),
            ephemeral_key: EphemeralKey::from([byte ^ 4; 32]),
            enc_ciphertext: CompactCiphertext::from([byte ^ 5; CompactCiphertext::LENGTH]),
        }
    }

    fn zats(value: u64) -> Zatoshis {
        Zatoshis::new(value).expect("in range")
    }

    /// A shielded transaction spending `spent` transparent zatoshis into
    /// `sapling` balance `balance`, with the given elements.
    fn shielded(
        n: u32,
        spent: u64,
        balance: i64,
        byte: u8,
        orchard: usize,
        ironwood: usize,
    ) -> FoldTx {
        let mut compact = tx(n).compact().clone();
        compact.transparent_inputs = vec![TransparentInput {
            prev_txid: TransactionId::from([9; 32]),
            prev_index: 0,
        }];
        compact.transparent_outputs = vec![TransparentOutput {
            value: zats(1),
            script: Script::from(vec![]),
        }];
        compact.sapling_nullifiers = vec![Nullifier::from([byte; 32])];
        compact.sapling_outputs = vec![output(byte), output(byte.wrapping_add(1))];
        compact.orchard_actions = (0..orchard)
            .map(|i| action(byte.wrapping_add(i as u8)))
            .collect();
        compact.ironwood_actions = (0..ironwood)
            .map(|i| action(byte ^ 0x80 ^ i as u8))
            .collect();
        let balances = PoolBalances {
            sapling: Some(SignedZatoshis::try_new(balance).expect("in range")),
            orchard: None,
            ironwood: None,
        };
        let prevouts = vec![TransparentOutput {
            value: zats(spent),
            script: Script::from(vec![]),
        }];
        FoldTx::try_new(compact, balances, prevouts).expect("paired")
    }

    /// Layouts, fees and every column read back per height; positions run
    /// on across heights and survive a resume.
    #[test]
    fn columns_and_layouts_read_back_across_heights() {
        let chain = [
            block_with(0, vec![tx(0), shielded(1, 100, -60, 10, 1, 0)]),
            block_with(
                1,
                vec![
                    tx(2),
                    shielded(3, 50, 0, 20, 0, 2),
                    shielded(4, 10, 5, 30, 2, 1),
                ],
            ),
        ];
        let backend = backend(&SCHEMA);
        let state = fold_all::<CompactBlock>(&backend, &chain).expect("folded");
        let reader = backend.reader().expect("reader");
        assert_eq!(
            CompactBlock::resume(&reader).expect("resume"),
            Some(state.clone())
        );
        let reader = Reader::new(&reader);

        let one = reader
            .layout(Height::try_from(1).expect("1"))
            .expect("read")
            .expect("held");
        assert_eq!(
            one.starts,
            ColumnStarts {
                sapling_spends: 1,
                sapling_outputs: 2,
                orchard_actions: 1,
                ironwood_actions: 0
            }
        );
        assert_eq!(
            one.txs.iter().map(|tx| tx.fee).collect::<Vec<_>>(),
            vec![zats(0), zats(49), zats(14)]
        );
        assert_eq!(
            state.next,
            ColumnStarts {
                sapling_spends: 3,
                sapling_outputs: 6,
                orchard_actions: 3,
                ironwood_actions: 3
            }
        );
        let zero = reader
            .layout(Height::try_from(0).expect("0"))
            .expect("read")
            .expect("held");
        assert_eq!(zero.txs[1].fee, zats(39));

        let mut expected_outputs = Vec::new();
        let mut expected_orchard = Vec::new();
        let mut expected_ironwood = Vec::new();
        for block in &chain {
            for tx in block.transactions() {
                expected_outputs.extend(tx.compact().sapling_outputs.iter().cloned());
                expected_orchard.extend(tx.compact().orchard_actions.iter().cloned());
                expected_ironwood.extend(tx.compact().ironwood_actions.iter().cloned());
            }
        }
        assert_eq!(
            reader.sapling_outputs(0, 6).expect("read"),
            expected_outputs
        );
        assert_eq!(
            reader.actions(ShieldedPool::Orchard, 0, 3).expect("read"),
            expected_orchard
        );
        assert_eq!(
            reader.actions(ShieldedPool::Ironwood, 0, 3).expect("read"),
            expected_ironwood
        );
        assert_eq!(
            reader.sapling_nullifiers(1, 2).expect("read"),
            vec![Nullifier::from([20; 32]), Nullifier::from([30; 32])]
        );
        assert!(
            reader.sapling_outputs(5, 2).is_err(),
            "past the column's end"
        );
    }

    /// A transaction paying out more than it takes in is refused.
    #[test]
    fn a_negative_fee_is_refused() {
        let block = block_with(0, vec![tx(0), shielded(1, 1, -5, 1, 0, 0)]);
        let folded = CompactBlock::fold(None, &block);
        assert!(
            matches!(folded, Err(FoldError::Continuity { index: NAME, .. })),
            "{folded:?}"
        );
    }

    /// The schema digest every `compact_block_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0x50, 0xbb, 0x1e, 0x4b, 0xd1, 0xd5, 0x42, 0xbd, 0xe8, 0x49, 0xf5, 0x1f, 0x71, 0x8a,
            0xf4, 0x58, 0xfc, 0xb5, 0x85, 0x55, 0x80, 0x79, 0x5e, 0xa6, 0xf0, 0xa0, 0xf2, 0x1c,
            0x31, 0x82, 0x91, 0x1f,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "compact_block_1");
    }
}
