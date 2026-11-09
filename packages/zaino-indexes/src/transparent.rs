//! I-04 `transparent`: every transparent input and output by location, in
//! wire form (D-24c).
//!
//! Three columns, one record each per height:
//! - `t_layout`: per transaction, the cumulative input and output counts
//!   through it, so a transaction's slice of the other two is two offsets;
//! - `t_in`: every input's prevout (txid, index), 36 bytes;
//! - `t_out`: every output's value (`u64` little-endian) then script.
//!
//! The one transparent-by-location store: compact blocks compose their
//! transparent axis from it, and prevouts resolve through it.

use zaino_persistence::{sequence_key, BackendReader, Namespace, WriteOp};
use zaino_primitives::types::{
    BlockRef, BlockTxPosition, Height, OutputIndex, Script, TransparentData, TransparentInput,
    TransparentOutput, Zatoshis,
};

use crate::block::FoldBlock;
use crate::codec::{
    self, elements, le_u32, le_u64, outpoint_bytes, var_ends_range, var_item_range, var_items,
    var_list, OUTPOINT_BYTES,
};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout, LayoutError};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "transparent";

const T_LAYOUT: Namespace = Namespace::new("t_layout");
const T_IN: Namespace = Namespace::new("t_in");
const T_OUT: Namespace = Namespace::new("t_out");

/// Bytes of a `t_layout` element: inputs then outputs through the
/// transaction, `u32` little-endian each.
const SPAN_BYTES: usize = 8;
/// Bytes of an output's value, ahead of its script.
const VALUE_BYTES: usize = 8;

declared! {
    /// Per transaction, cumulative input and output counts.
    pub const LAYOUT: Layout = Layout::new(
        "t_layout",
        1,
        &[Field::fixed("count", 4), Field::var("spans")],
    );
}
declared! {
    /// Every input's prevout, in block order.
    pub const INPUTS: Layout = Layout::new(
        "t_in",
        1,
        &[Field::fixed("count", 4), Field::var("prevouts")],
    );
}
declared! {
    /// Every output, in block order: offsets then (value, script) items.
    pub const OUTPUTS: Layout = Layout::new(
        "t_out",
        1,
        &[Field::fixed("count", 4), Field::var("outputs")],
    );
}
const COUNT: usize = 0;
const LIST: usize = 1;
/// Where every column's list starts: after its 4-byte count.
const LIST_START: usize = 4;
const _: () = assert!(LAYOUT.fixed_len() == LIST_START);
const _: () = assert!(INPUTS.fixed_len() == LIST_START);
const _: () = assert!(OUTPUTS.fixed_len() == LIST_START);

/// The schema: the three columns.
pub const SCHEMA: Schema = Schema::new(
    NAME,
    1,
    &[
        NamespaceLayout::sequence(T_LAYOUT, &LAYOUT),
        NamespaceLayout::sequence(T_IN, &INPUTS),
        NamespaceLayout::sequence(T_OUT, &OUTPUTS),
    ],
);

/// I-04.
pub struct Transparent;

/// The last stored block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
}

impl Index for Transparent {
    const SCHEMA: Schema = SCHEMA;
    const READS: &'static [&'static str] = &[];
    const BUILDS_FROM: &'static [&'static str] = &[];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        if backend
            .with_value(T_LAYOUT, &key(tip.height), |_| ())?
            .is_none()
        {
            return Err(FoldError::continuity(NAME, "tip without its layout"));
        }
        Ok(Some(State { tip }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let mut spans = Vec::with_capacity(block.transactions().len().saturating_mul(SPAN_BYTES));
        let mut prevouts = Vec::new();
        let mut outputs = Vec::new();
        let (mut inputs_through, mut outputs_through) = (0usize, 0usize);
        for tx in block.transactions() {
            let compact = tx.compact();
            for input in &compact.transparent_inputs {
                prevouts.extend_from_slice(&outpoint_bytes(outpoint(input)));
            }
            for output in &compact.transparent_outputs {
                let mut item =
                    Vec::with_capacity(VALUE_BYTES.saturating_add(output.script.as_bytes().len()));
                item.extend_from_slice(&u64::from(output.value).to_le_bytes());
                item.extend_from_slice(output.script.as_bytes());
                outputs.push(item);
            }
            inputs_through = inputs_through
                .checked_add(compact.transparent_inputs.len())
                .ok_or(LayoutError::Overflow)?;
            outputs_through = outputs_through
                .checked_add(compact.transparent_outputs.len())
                .ok_or(LayoutError::Overflow)?;
            spans.extend_from_slice(&codec::count(inputs_through)?.to_le_bytes());
            spans.extend_from_slice(&codec::count(outputs_through)?.to_le_bytes());
        }
        let items: Vec<&[u8]> = outputs.iter().map(Vec::as_slice).collect();
        let tx_count = codec::count(block.transactions().len())?;
        let reference = block.reference();
        let key = key(reference.height);
        let put = |namespace, value| WriteOp::Put {
            namespace,
            key: key.clone(),
            value,
        };
        Ok(Fold {
            ops: vec![
                put(T_LAYOUT, LAYOUT.encode(&[&tx_count.to_le_bytes(), &spans])?),
                put(
                    T_IN,
                    INPUTS.encode(&[&codec::count(inputs_through)?.to_le_bytes(), &prevouts])?,
                ),
                put(
                    T_OUT,
                    OUTPUTS.encode(&[
                        &codec::count(outputs_through)?.to_le_bytes(),
                        &var_list(&items)?,
                    ])?,
                ),
            ],
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

    /// Every transaction's transparent inputs and outputs at `height`, in
    /// block order.
    pub fn transactions(&self, height: Height) -> Result<Option<Vec<TransparentData>>, FoldError> {
        let key = key(height);
        let Some(layout) = self.backend.get(T_LAYOUT, &key)? else {
            return Ok(None);
        };
        let (Some(inputs), Some(outputs)) = (
            self.backend.get(T_IN, &key)?,
            self.backend.get(T_OUT, &key)?,
        ) else {
            return Err(FoldError::continuity(
                NAME,
                format!("columns missing at {height}"),
            ));
        };
        let spans = elements(
            NAME,
            record_count(&LAYOUT, &layout)?,
            LAYOUT.field(&layout, LIST)?,
            SPAN_BYTES,
        )?;
        let prevouts = elements(
            NAME,
            record_count(&INPUTS, &inputs)?,
            INPUTS.field(&inputs, LIST)?,
            OUTPOINT_BYTES,
        )?
        .map(prevout)
        .collect::<Result<Vec<_>, _>>()?;
        let outputs = var_items(
            NAME,
            record_count(&OUTPUTS, &outputs)?,
            OUTPUTS.field(&outputs, LIST)?,
        )?
        .into_iter()
        .map(output)
        .collect::<Result<Vec<_>, _>>()?;
        let (mut prevouts, mut outputs) = (prevouts.into_iter(), outputs.into_iter());
        let (mut inputs_before, mut outputs_before) = (0usize, 0usize);
        spans
            .map(|span| {
                let (inputs_through, outputs_through) = span_counts(span)?;
                let take = |before: usize, through: usize| {
                    through.checked_sub(before).ok_or_else(|| {
                        FoldError::continuity(NAME, format!("layout runs backwards at {height}"))
                    })
                };
                let tx = TransparentData {
                    inputs: prevouts
                        .by_ref()
                        .take(take(inputs_before, inputs_through)?)
                        .collect(),
                    outputs: outputs
                        .by_ref()
                        .take(take(outputs_before, outputs_through)?)
                        .collect(),
                };
                (inputs_before, outputs_before) = (inputs_through, outputs_through);
                Ok(tx)
            })
            .collect::<Result<Vec<_>, FoldError>>()
            .and_then(
                |txs| match prevouts.next().is_none() && outputs.next().is_none() {
                    true => Ok(Some(txs)),
                    false => Err(FoldError::continuity(
                        NAME,
                        format!("columns disagree at {height}"),
                    )),
                },
            )
    }

    /// Output `vout` of the transaction at `position`: a handful of field
    /// reads, never a whole record. `None` where there is no such output.
    pub fn output(
        &self,
        position: BlockTxPosition,
        vout: OutputIndex,
    ) -> Result<Option<TransparentOutput>, FoldError> {
        let key = key(position.height);
        let Some(tx_count) = self.count(T_LAYOUT, &LAYOUT, &key)? else {
            return Ok(None);
        };
        if position.tx_index >= tx_count {
            return Ok(None);
        }
        let tx_index = usize::try_from(position.tx_index).map_err(|_| LayoutError::Overflow)?;
        let first = tx_index.saturating_sub(1);
        let wanted = LAYOUT.element_range(LIST, first, SPAN_BYTES)?;
        let range = match tx_index {
            0 => wanted,
            _ => wanted.start..wanted.end.saturating_add(SPAN_BYTES),
        };
        let spans = self.range(T_LAYOUT, &key, range)?;
        let (before, through) = match spans.split_at_checked(SPAN_BYTES) {
            Some((previous, own)) if tx_index > 0 => {
                (span_counts(previous)?.1, span_counts(own)?.1)
            }
            _ => (0, span_counts(&spans)?.1),
        };
        let vout = usize::try_from(vout).map_err(|_| LayoutError::Overflow)?;
        let Some(item) = before.checked_add(vout).filter(|item| *item < through) else {
            return Ok(None);
        };
        let Some(output_count) = self.count(T_OUT, &OUTPUTS, &key)? else {
            return Err(FoldError::continuity(
                NAME,
                format!("outputs missing at {}", position.height),
            ));
        };
        let ends = self.range(T_OUT, &key, var_ends_range(LIST_START, item)?)?;
        let item_range = var_item_range(LIST_START, output_count, item, &ends)?;
        output(&self.range(T_OUT, &key, item_range)?).map(Some)
    }

    fn count(
        &self,
        namespace: Namespace,
        layout: &Layout,
        key: &[u8],
    ) -> Result<Option<u32>, FoldError> {
        let range = layout.fixed_range(COUNT)?;
        match self.backend.get_bytes(namespace, key, range)? {
            Some(bytes) => Ok(Some(le_u32(&bytes, "count")?)),
            None => Ok(None),
        }
    }

    fn range(
        &self,
        namespace: Namespace,
        key: &[u8],
        range: core::ops::Range<usize>,
    ) -> Result<Vec<u8>, FoldError> {
        self.backend
            .get_bytes(namespace, key, range)?
            .ok_or_else(|| FoldError::continuity(NAME, "record vanished between reads"))
    }
}

fn key(height: Height) -> Vec<u8> {
    sequence_key(u64::from(height))
}

fn record_count(layout: &Layout, record: &[u8]) -> Result<u32, FoldError> {
    Ok(le_u32(layout.field(record, COUNT)?, "count")?)
}

fn outpoint(input: &TransparentInput) -> zaino_primitives::types::Outpoint {
    zaino_primitives::types::Outpoint {
        txid: input.prev_txid,
        index: input.prev_index,
    }
}

fn span_counts(span: &[u8]) -> Result<(usize, usize), FoldError> {
    let (inputs, outputs) = span.split_at_checked(4).ok_or(LayoutError::FieldWidth {
        field: "span",
        expected: SPAN_BYTES,
        actual: span.len(),
    })?;
    let to_usize = |bytes, field| -> Result<usize, FoldError> {
        usize::try_from(le_u32(bytes, field)?).map_err(|_| FoldError::from(LayoutError::Overflow))
    };
    Ok((to_usize(inputs, "inputs")?, to_usize(outputs, "outputs")?))
}

fn prevout(bytes: &[u8]) -> Result<TransparentInput, FoldError> {
    let (txid, index) = bytes.split_at_checked(32).ok_or(LayoutError::FieldWidth {
        field: "prevout",
        expected: OUTPOINT_BYTES,
        actual: bytes.len(),
    })?;
    let index = codec::array::<4>(index, "prev_index").map(u32::from_be_bytes)?;
    Ok(TransparentInput {
        prev_txid: codec::txid(txid)?,
        prev_index: index,
    })
}

fn output(bytes: &[u8]) -> Result<TransparentOutput, FoldError> {
    let (value, script) = bytes
        .split_at_checked(VALUE_BYTES)
        .ok_or(LayoutError::Truncated {
            field: "value",
            needed: VALUE_BYTES,
            len: bytes.len(),
        })?;
    let value = le_u64(value, "value")?;
    let value = Zatoshis::new(value)
        .map_err(|_| FoldError::continuity(NAME, format!("stored value {value}")))?;
    Ok(TransparentOutput {
        value,
        script: Script::from(script.to_vec()),
    })
}

#[cfg(test)]
mod tests {
    use zaino_persistence::{Backend as _, SchemaDigest};
    use zaino_primitives::types::{PoolBalances, TransactionId};

    use super::*;
    use crate::block::FoldTx;
    use crate::testing::{backend, block_with, fold_all, tx};

    fn out(value: u64, script: &[u8]) -> TransparentOutput {
        TransparentOutput {
            value: Zatoshis::new(value).expect("in range"),
            script: Script::from(script.to_vec()),
        }
    }

    fn spend(txid: u8, index: u32) -> TransparentInput {
        TransparentInput {
            prev_txid: TransactionId::from([txid; 32]),
            prev_index: index,
        }
    }

    /// A transaction with `inputs` (prevouts are irrelevant to this index)
    /// and `outputs`.
    fn with(n: u32, inputs: Vec<TransparentInput>, outputs: Vec<TransparentOutput>) -> FoldTx {
        let mut compact = tx(n).compact().clone();
        let prevouts = inputs.iter().map(|_| out(1, b"p")).collect();
        compact.transparent_inputs = inputs;
        compact.transparent_outputs = outputs;
        FoldTx::try_new(compact, PoolBalances::default(), Some(prevouts)).expect("paired")
    }

    /// Whole-block and per-output reads return exactly what was folded,
    /// across coinbase, shielded-only and multi-input/output transactions.
    #[test]
    fn transparent_data_reads_back_by_block_and_by_output() {
        let txs = vec![
            with(0, vec![], vec![out(50, b"coinbase")]),
            with(1, vec![], vec![]),
            with(
                2,
                vec![spend(7, 0), spend(8, 3)],
                vec![out(5, b""), out(6, b"xy")],
            ),
            with(
                3,
                vec![spend(9, 1)],
                vec![out(u64::from(u32::MAX) + 1, b"big")],
            ),
        ];
        let block = block_with(0, txs);
        let backend = backend(&SCHEMA);
        fold_all::<Transparent>(&backend, std::slice::from_ref(&block)).expect("folded");
        let reader = backend.reader().expect("reader");
        let reader = Reader::new(&reader);

        let height = block.header().height;
        let expected: Vec<TransparentData> = block
            .transactions()
            .iter()
            .map(|tx| TransparentData {
                inputs: tx.compact().transparent_inputs.clone(),
                outputs: tx.compact().transparent_outputs.clone(),
            })
            .collect();
        let read = reader.transactions(height).expect("read").expect("present");
        assert_eq!(read.len(), expected.len());
        for (read, expected) in read.iter().zip(&expected) {
            assert_eq!(read.inputs, expected.inputs);
            assert_eq!(read.outputs, expected.outputs);
        }
        for (tx_index, tx) in (0u32..).zip(&expected) {
            let at = BlockTxPosition::new(height, tx_index);
            for (vout, output) in (0u32..).zip(&tx.outputs) {
                assert_eq!(
                    reader.output(at, vout).expect("read").as_ref(),
                    Some(output)
                );
            }
            let past = codec::count(tx.outputs.len()).expect("small");
            assert_eq!(reader.output(at, past).expect("read"), None);
        }
        assert_eq!(
            reader
                .output(BlockTxPosition::new(height, 4), 0)
                .expect("read"),
            None
        );
        let above = Height::try_from(1).expect("1");
        assert_eq!(
            reader
                .transactions(above)
                .expect("read")
                .map(|txs| txs.len()),
            None
        );
    }

    /// The schema digest every `transparent_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0xf6, 0x4a, 0xcb, 0xf8, 0x66, 0x91, 0x9d, 0xea, 0xa4, 0xa7, 0xcc, 0x2e, 0x4a, 0x9a,
            0xdc, 0x88, 0x2d, 0x13, 0x44, 0x07, 0x90, 0x0e, 0x67, 0x6a, 0x9f, 0x42, 0xdd, 0x29,
            0xa9, 0x49, 0x9d, 0x1a,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "transparent_1");
    }
}
