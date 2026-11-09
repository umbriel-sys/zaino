//! I-11 `tx_out_set`: `gettxoutsetinfo` at every height, without a scan.
//!
//! The set is zcashd's (D-70): every unspent transparent output except those
//! `is_unspendable` names, never the genesis coinbase's (consensus makes them
//! unspendable). The fold keeps the `TxOutSetAccumulator` and, per
//! transaction still holding counted outputs, how many remain (D-69); the
//! count's 0 ↔ non-zero transitions step `transactions`. Counts persist as
//! `txid ‖ remaining` rows: a change deletes the old row and inserts the new,
//! and counts only fall, so no key is written twice.

use core::num::NonZeroUsize;
use core::ops::Bound;
use std::collections::HashSet;

use zaino_persistence::{
    sequence_key, BackendReader, Namespace, ScanDirection, ScanRange, WriteOp,
};
use zaino_primitives::txout_commitment::{commitment_address, is_unspendable, TxOutSetAccumulator};
use zaino_primitives::types::{BlockRef, Height, Outpoint, TransactionId};

use crate::block::{is_genesis, FoldBlock};
use crate::codec::{self, array, le_u64};
use crate::index::{check_extends, Fold, FoldError, Index};
use crate::layout::{declared, Field, Layout};
use crate::schema::{NamespaceLayout, Schema};

/// The index's name: its directory and the name dependants use.
pub const NAME: &str = "tx_out_set";

const ACCUMULATOR: Namespace = Namespace::new("accumulator");
const REMAINING: Namespace = Namespace::new("remaining");

declared! {
    /// The accumulator after a height: transactions, outputs, bytes, total
    /// (`u64` little-endian each), then the commitment.
    pub const TOTALS: Layout = Layout::new(
        "tx_out_set_totals",
        1,
        &[
            Field::fixed("transactions", 8),
            Field::fixed("outputs", 8),
            Field::fixed("bytes", 8),
            Field::fixed("total", 8),
            Field::fixed("hash", 32),
        ],
    );
}
declared! {
    /// A transaction's remaining counted outputs: txid, count big-endian.
    pub const REMAINING_KEY: Layout = Layout::new(
        "tx_remaining",
        1,
        &[Field::fixed("txid", 32), Field::fixed("remaining", 4)],
    );
}
declared! {
    /// The row's (empty) value.
    pub const PRESENT: Layout = Layout::new("present", 1, &[Field::fixed("present", 1)]);
}
declared! {
    const REMAINING_ROWS: NamespaceLayout =
        NamespaceLayout::map(REMAINING, &REMAINING_KEY, &PRESENT, true);
}
const KEY_BYTES: usize = 36;
const _: () = assert!(REMAINING_KEY.fixed_len() == KEY_BYTES);
/// Bytes a resume scan visits per chunk.
const CHUNK: NonZeroUsize = match NonZeroUsize::new(1 << 20) {
    Some(chunk) => chunk,
    None => NonZeroUsize::MIN,
};

/// The schema: totals per height, counts per transaction.
pub const SCHEMA: Schema = Schema::new(
    NAME,
    1,
    &[
        NamespaceLayout::sequence(ACCUMULATOR, &TOTALS),
        REMAINING_ROWS,
    ],
);

/// I-11.
pub struct TxOutSet;

/// The last stored block, the accumulator after it, and each transaction's
/// remaining counted outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// The last stored block.
    pub tip: BlockRef,
    /// The totals and commitment.
    pub accumulator: TxOutSetAccumulator,
    /// Remaining counted outputs per transaction holding any.
    pub remaining: imbl::HashMap<TransactionId, u32>,
}

impl Index for TxOutSet {
    const SCHEMA: Schema = SCHEMA;
    const READS: &'static [&'static str] = &[];
    const BUILDS_FROM: &'static [&'static str] =
        &[crate::txid_location::NAME, crate::transparent::NAME];
    type State = State;

    fn resume<R: BackendReader>(backend: &R) -> Result<Option<State>, FoldError> {
        let Some(tip) = backend.tip()? else {
            return Ok(None);
        };
        let accumulator = Reader::new(backend)
            .accumulator(tip.height)?
            .ok_or_else(|| FoldError::continuity(NAME, "tip without its totals"))?;
        let range = ScanRange {
            lower: Bound::Unbounded,
            upper: Bound::Unbounded,
            direction: ScanDirection::Forward,
            chunk_bytes: CHUNK,
        };
        let mut remaining = imbl::HashMap::new();
        for chunk in backend.scan_range(REMAINING, range, |key, _| Some(decode_key(key))) {
            for row in chunk? {
                let (txid, count) = row?;
                if count == 0 || remaining.insert(txid, count).is_some() {
                    return Err(FoldError::continuity(
                        NAME,
                        format!("bad count row for {txid}"),
                    ));
                }
            }
        }
        if codec::count(remaining.len()).map(u64::from).ok() != Some(accumulator.transactions) {
            return Err(FoldError::continuity(
                NAME,
                format!(
                    "{} count rows for {} transactions",
                    remaining.len(),
                    accumulator.transactions
                ),
            ));
        }
        Ok(Some(State {
            tip,
            accumulator,
            remaining,
        }))
    }

    fn fold(state: Option<&State>, block: &FoldBlock) -> Result<Fold<State>, FoldError> {
        check_extends(NAME, state.map(|state| state.tip), block)?;
        let mut accumulator = state.map_or(TxOutSetAccumulator::empty(), |state| state.accumulator);
        let mut remaining = state
            .map(|state| state.remaining.clone())
            .unwrap_or_default();
        // Each touched transaction's count before the block, in first-touch
        // order, so only the block's net change is written.
        let mut before: Vec<(TransactionId, Option<u32>)> = Vec::new();
        let mut seen: HashSet<TransactionId> = HashSet::new();
        let mut touch = |txid: TransactionId, remaining: &imbl::HashMap<TransactionId, u32>| {
            if seen.insert(txid) {
                before.push((txid, remaining.get(&txid).copied()));
            }
        };
        let txs = match is_genesis(block) {
            true => &[][..],
            false => block.transactions(),
        };
        for tx in txs {
            let compact = tx.compact();
            let prevouts = tx.resolved_prevouts(NAME)?;
            for (input, prevout) in compact.transparent_inputs.iter().zip(prevouts) {
                let script = prevout.script.as_bytes();
                if is_unspendable(script) {
                    continue;
                }
                let outpoint = Outpoint {
                    txid: input.prev_txid,
                    index: input.prev_index,
                };
                accumulator.apply_removed_output(
                    &outpoint,
                    prevout.value,
                    &commitment_address(script),
                )?;
                touch(outpoint.txid, &remaining);
                match remaining.get(&outpoint.txid).copied() {
                    Some(1) => {
                        remaining.remove(&outpoint.txid);
                        accumulator.transactions = accumulator
                            .transactions
                            .checked_sub(1)
                            .ok_or_else(|| FoldError::continuity(NAME, "transactions underflow"))?;
                    }
                    Some(count) => {
                        remaining.insert(outpoint.txid, count.saturating_sub(1));
                    }
                    None => {
                        return Err(FoldError::continuity(
                            NAME,
                            format!(
                                "spend of {}:{} which holds no counted output",
                                outpoint.txid, outpoint.index
                            ),
                        ))
                    }
                }
            }
            let mut created: u32 = 0;
            for (index, output) in (0u32..).zip(&compact.transparent_outputs) {
                let script = output.script.as_bytes();
                if is_unspendable(script) {
                    continue;
                }
                let outpoint = Outpoint {
                    txid: compact.txid,
                    index,
                };
                accumulator.apply_added_output(
                    &outpoint,
                    output.value,
                    &commitment_address(script),
                )?;
                created = created
                    .checked_add(1)
                    .ok_or_else(|| FoldError::continuity(NAME, "output count overflow"))?;
            }
            if created > 0 {
                touch(compact.txid, &remaining);
                if remaining.insert(compact.txid, created).is_some() {
                    return Err(FoldError::continuity(
                        NAME,
                        format!("txid {} seen twice", compact.txid),
                    ));
                }
                accumulator.transactions = accumulator
                    .transactions
                    .checked_add(1)
                    .ok_or_else(|| FoldError::continuity(NAME, "transactions overflow"))?;
            }
        }
        let mut ops = Vec::new();
        for (txid, was) in before {
            let now = remaining.get(&txid).copied();
            if was == now {
                continue;
            }
            if let Some(was) = was {
                ops.push(WriteOp::Delete {
                    namespace: REMAINING,
                    key: key(txid, was).to_vec(),
                });
            }
            if let Some(now) = now {
                ops.push(WriteOp::Put {
                    namespace: REMAINING,
                    key: key(txid, now).to_vec(),
                    value: vec![0],
                });
            }
        }
        let reference = block.reference();
        ops.push(WriteOp::Put {
            namespace: ACCUMULATOR,
            key: sequence_key(u64::from(reference.height)),
            value: TOTALS.encode(&[
                &accumulator.transactions.to_le_bytes(),
                &accumulator.transaction_outputs.to_le_bytes(),
                &accumulator.bytes_serialized.to_le_bytes(),
                &accumulator.total_zatoshis.to_le_bytes(),
                &accumulator.hash_serialized,
            ])?,
        });
        Ok(Fold {
            ops,
            next: State {
                tip: reference,
                accumulator,
                remaining,
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

    /// The accumulator after `height`: one read.
    pub fn accumulator(&self, height: Height) -> Result<Option<TxOutSetAccumulator>, FoldError> {
        let Some(record) = self
            .backend
            .get(ACCUMULATOR, &sequence_key(u64::from(height)))?
        else {
            return Ok(None);
        };
        let field = |index| TOTALS.field(&record, index);
        Ok(Some(TxOutSetAccumulator {
            transactions: le_u64(field(0)?, "transactions")?,
            transaction_outputs: le_u64(field(1)?, "outputs")?,
            bytes_serialized: le_u64(field(2)?, "bytes")?,
            total_zatoshis: le_u64(field(3)?, "total")?,
            hash_serialized: array::<32>(field(4)?, "hash")?,
        }))
    }
}

fn key(txid: TransactionId, remaining: u32) -> [u8; KEY_BYTES] {
    let mut out = [0; KEY_BYTES];
    let (head, tail) = out.split_at_mut(32);
    head.copy_from_slice(&<[u8; 32]>::from(txid));
    tail.copy_from_slice(&remaining.to_be_bytes());
    out
}

fn decode_key(key: &[u8]) -> Result<(TransactionId, u32), FoldError> {
    let key = array::<KEY_BYTES>(key, "tx_remaining")?;
    let (txid, count) = key.split_at(32);
    Ok((
        codec::txid(txid)?,
        u32::from_be_bytes(array::<4>(count, "remaining")?),
    ))
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};

    use zaino_persistence::{Backend as _, MaintenanceReader as _, SchemaDigest};
    use zaino_primitives::txout_commitment::{entry_digest, TXOUT_SET_ENTRY_LEN};
    use zaino_primitives::types::{
        PoolBalances, Script, TransparentInput, TransparentOutput, Zatoshis,
    };

    use super::*;
    use crate::block::FoldTx;
    use crate::testing::{backend, block_with, fold_all, tx};

    /// A deterministic pseudo-random sequence.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, bound: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.0 >> 33) % bound
        }
    }

    /// A script of each kind the set rule distinguishes.
    fn script(kind: u64, seed: u8) -> Script {
        Script::from(match kind {
            0 => [&[0x76, 0xa9, 0x14][..], &[seed; 20], &[0x88, 0xac]].concat(),
            1 => [&[0xa9, 0x14][..], &[seed; 20], &[0x87]].concat(),
            2 => [&[33][..], &[seed; 33], &[0xac]].concat(),
            3 => vec![0x6a, 2, seed, seed],
            _ => vec![0x51; MAX_SCRIPT + 1],
        })
    }
    const MAX_SCRIPT: usize = zaino_primitives::txout_commitment::MAX_SCRIPT_SIZE;

    fn spending(
        txid_seed: u32,
        spends: Vec<(Outpoint, TransparentOutput)>,
        outputs: Vec<TransparentOutput>,
    ) -> FoldTx {
        let mut compact = tx(txid_seed).compact().clone();
        compact.transparent_inputs = spends
            .iter()
            .map(|(outpoint, _)| TransparentInput {
                prev_txid: outpoint.txid,
                prev_index: outpoint.index,
            })
            .collect();
        compact.transparent_outputs = outputs;
        let prevouts = spends.into_iter().map(|(_, output)| output).collect();
        FoldTx::try_new(compact, PoolBalances::default(), Some(prevouts)).expect("paired")
    }

    /// The set recomputed from scratch, as zcashd defines it.
    fn oracle(utxos: &BTreeMap<(TransactionId, u32), TransparentOutput>) -> TxOutSetAccumulator {
        let mut out = TxOutSetAccumulator::empty();
        let mut holders: HashSet<TransactionId> = HashSet::new();
        for ((txid, index), output) in utxos {
            let script = output.script.as_bytes();
            if is_unspendable(script) {
                continue;
            }
            holders.insert(*txid);
            let digest = entry_digest(
                &Outpoint {
                    txid: *txid,
                    index: *index,
                },
                output.value,
                &commitment_address(script),
            );
            for (dst, src) in out.hash_serialized.iter_mut().zip(digest) {
                *dst ^= src;
            }
            out.transaction_outputs += 1;
            out.total_zatoshis += u64::from(output.value);
        }
        out.bytes_serialized = out.transaction_outputs * TXOUT_SET_ENTRY_LEN;
        out.transactions = holders.len() as u64;
        out
    }

    /// Every height of a random chain equals the from-scratch oracle; the
    /// count rows match the state; a resume reproduces it; genesis outputs
    /// never enter the set.
    #[test]
    fn every_height_matches_the_oracle() {
        let mut rng = Lcg(7);
        let mut utxos: BTreeMap<(TransactionId, u32), TransparentOutput> = BTreeMap::new();
        let mut blocks = Vec::new();
        let mut expected = Vec::new();
        let mut seed = 0u32;
        for n in 0..40u32 {
            let mut txs = vec![];
            let coinbase_outputs: Vec<_> = (0..1 + rng.next(3))
                .map(|i| TransparentOutput {
                    value: Zatoshis::new(1_000 + rng.next(1_000)).expect("value"),
                    script: script(rng.next(5), i as u8),
                })
                .collect();
            seed += 1;
            let coinbase = spending(seed, vec![], coinbase_outputs);
            let mut created: Vec<(Outpoint, TransparentOutput)> = Vec::new();
            for (index, output) in (0u32..).zip(&coinbase.compact().transparent_outputs) {
                created.push((
                    Outpoint {
                        txid: coinbase.compact().txid,
                        index,
                    },
                    output.clone(),
                ));
            }
            txs.push(coinbase);
            for _ in 0..rng.next(4) {
                // Spend from older blocks and from earlier in this block.
                let mut spends = Vec::new();
                for _ in 0..rng.next(4) {
                    let pool: Vec<(Outpoint, TransparentOutput)> = utxos
                        .iter()
                        .filter(|_| n > 0)
                        .map(|((txid, index), output)| {
                            (
                                Outpoint {
                                    txid: *txid,
                                    index: *index,
                                },
                                output.clone(),
                            )
                        })
                        .chain(created.iter().cloned())
                        .filter(|(outpoint, _)| {
                            !spends
                                .iter()
                                .any(|(s, _): &(Outpoint, TransparentOutput)| s == outpoint)
                        })
                        .collect();
                    if pool.is_empty() {
                        break;
                    }
                    let pick = pool[rng.next(pool.len() as u64) as usize].clone();
                    spends.push(pick);
                }
                let outputs: Vec<_> = (0..rng.next(3))
                    .map(|i| TransparentOutput {
                        value: Zatoshis::new(rng.next(500)).expect("value"),
                        script: script(rng.next(5), i as u8 ^ 0x55),
                    })
                    .collect();
                seed += 1;
                let tx = spending(seed, spends.clone(), outputs);
                for (outpoint, _) in &spends {
                    utxos.remove(&(outpoint.txid, outpoint.index));
                    created.retain(|(created, _)| created != outpoint);
                }
                for (index, output) in (0u32..).zip(&tx.compact().transparent_outputs) {
                    created.push((
                        Outpoint {
                            txid: tx.compact().txid,
                            index,
                        },
                        output.clone(),
                    ));
                }
                txs.push(tx);
            }
            if n > 0 {
                for (outpoint, output) in created {
                    utxos.insert((outpoint.txid, outpoint.index), output);
                }
            }
            blocks.push(block_with(n, txs));
            expected.push(oracle(&utxos));
        }
        let backend = backend(&SCHEMA);
        let state = fold_all::<TxOutSet>(&backend, &blocks).expect("folded");
        let snapshot = backend.reader().expect("reader");
        let reader = Reader::new(&snapshot);
        for (n, expected) in (0u32..).zip(&expected) {
            let height = Height::try_from(n).expect("h");
            assert_eq!(
                reader.accumulator(height).expect("read"),
                Some(*expected),
                "height {n}"
            );
        }
        assert_eq!(
            expected[0],
            TxOutSetAccumulator::empty(),
            "genesis outputs are not in the set"
        );
        let rows: HashMap<TransactionId, u32> = snapshot
            .scan(REMAINING)
            .expect("scan")
            .into_iter()
            .map(|(key, _)| decode_key(&key).expect("row"))
            .collect();
        let held: HashMap<TransactionId, u32> =
            state.remaining.iter().map(|(k, v)| (*k, *v)).collect();
        assert_eq!(rows, held, "rows mirror the state");
        assert!(
            state.accumulator.transactions > 0,
            "the chain left some set"
        );
        assert_eq!(TxOutSet::resume(&snapshot).expect("resume"), Some(state));
    }

    /// Spending an output the set does not hold is refused.
    #[test]
    fn spending_an_untracked_output_is_refused() {
        let backend = backend(&SCHEMA);
        let state = fold_all::<TxOutSet>(&backend, &[block_with(0, vec![tx(0)])]).expect("genesis");
        let ghost = (
            Outpoint {
                txid: TransactionId::from([9; 32]),
                index: 0,
            },
            TransparentOutput {
                value: Zatoshis::new(5).expect("5"),
                script: script(0, 1),
            },
        );
        let block = block_with(1, vec![tx(1), spending(2, vec![ghost], vec![])]);
        assert!(matches!(
            TxOutSet::fold(Some(&state), &block),
            Err(FoldError::TxOutSet(_)) | Err(FoldError::Continuity { .. })
        ));
    }

    /// The schema digest every `tx_out_set_1` directory carries.
    #[test]
    fn the_schema_digest_is_golden() {
        const GOLDEN: [u8; 32] = [
            0xe3, 0xce, 0xc4, 0x93, 0x06, 0xe7, 0x06, 0xe5, 0x75, 0x2c, 0xd2, 0x62, 0x9d, 0x78,
            0xd9, 0x81, 0x86, 0x83, 0x19, 0xad, 0x10, 0xb3, 0x59, 0x5f, 0x90, 0x25, 0x2b, 0x00,
            0xf3, 0x9f, 0x86, 0x36,
        ];
        assert_eq!(
            SCHEMA.digest(),
            SchemaDigest::new(GOLDEN),
            "{:02x?}",
            SCHEMA.digest().as_bytes()
        );
        assert_eq!(SCHEMA.dir_name(), "tx_out_set_1");
    }
}
