//! Domain compact blocks on the light-wallet wire: the prost conversions and
//! a direct writer that emits the same bytes with no intermediate structs.
//!
//! Free functions, not `to_wire` methods: both sides are foreign to this
//! crate, and the writer must sit below every crate that serves compact
//! blocks.
//!
//! Wire rules, shared by the conversion and the writer:
//! - transactions with nothing in a requested pool are dropped; `index` keeps
//!   each survivor's position in the dense domain block;
//! - `fee` is the domain fee saturated to `u32`, unset (0) where unknown;
//! - a block read under [`PoolFilter::nullifiers_only`] carries zero tree
//!   sizes (the protocol's nullifier responses include none); every other
//!   block carries its real sizes.

use prost::encoding::{encode_key, encode_varint, encoded_len_varint, key_len, WireType};
use zaino_primitives::types::{
    ActionOutput, ChainMetadata, CompactAction, CompactBlock, PoolFilter, PreIndexCompactTx,
    SaplingOutput, ShieldedPool, TransparentInput, TransparentOutput, Zatoshis,
};

use crate::proto::compact_formats as proto;
use crate::proto::utils::PoolTypeFilter;

/// A domain compact block read under `pools`, as the protocol's message.
pub fn compact_block_to_wire(block: &CompactBlock, pools: PoolFilter) -> proto::CompactBlock {
    let [sapling, orchard, ironwood] = tree_sizes(&block.chain_metadata, pools);
    proto::CompactBlock {
        height: u64::from(block.height),
        hash: <[u8; 32]>::from(block.hash).to_vec(),
        prev_hash: <[u8; 32]>::from(block.prev_hash).to_vec(),
        time: block.time,
        header: Vec::new(),
        vtx: served(block)
            .map(|(index, tx)| compact_tx_to_wire(index, tx))
            .collect(),
        chain_metadata: Some(proto::ChainMetadata {
            sapling_commitment_tree_size: sapling,
            orchard_commitment_tree_size: orchard,
            ironwood_commitment_tree_size: ironwood,
        }),
    }
}

/// A compact transaction at `index` in its block, as the protocol's message.
pub fn compact_tx_to_wire(index: usize, tx: &PreIndexCompactTx) -> proto::CompactTx {
    proto::CompactTx {
        index: index as u64,
        txid: <[u8; 32]>::from(tx.txid).to_vec(),
        fee: fee(tx.fee),
        spends: tx
            .sapling_nullifiers
            .iter()
            .map(|nullifier| proto::CompactSaplingSpend {
                nf: <[u8; 32]>::from(*nullifier).to_vec(),
            })
            .collect(),
        outputs: tx
            .sapling_outputs
            .iter()
            .map(|output| proto::CompactSaplingOutput {
                cmu: <[u8; 32]>::from(output.cmu).to_vec(),
                ephemeral_key: <[u8; 32]>::from(output.ephemeral_key).to_vec(),
                ciphertext: <[u8; 52]>::from(output.enc_ciphertext).to_vec(),
            })
            .collect(),
        actions: tx.orchard_actions.iter().map(action_to_wire).collect(),
        ironwood_actions: tx.ironwood_actions.iter().map(action_to_wire).collect(),
        vin: tx
            .transparent_inputs
            .iter()
            .map(|input| proto::CompactTxIn {
                prevout_txid: <[u8; 32]>::from(input.prev_txid).to_vec(),
                prevout_index: input.prev_index,
            })
            .collect(),
        vout: tx
            .transparent_outputs
            .iter()
            .map(|output| proto::TxOut {
                value: u64::from(output.value),
                script_pub_key: output.script.as_bytes().to_vec(),
            })
            .collect(),
    }
}

fn action_to_wire(action: &CompactAction) -> proto::CompactOrchardAction {
    let output = action.output.as_ref();
    proto::CompactOrchardAction {
        nullifier: <[u8; 32]>::from(action.nullifier).to_vec(),
        cmx: output.map_or_else(Vec::new, |o| <[u8; 32]>::from(o.cmx).to_vec()),
        ephemeral_key: output.map_or_else(Vec::new, |o| <[u8; 32]>::from(o.ephemeral_key).to_vec()),
        ciphertext: output.map_or_else(Vec::new, |o| <[u8; 52]>::from(o.enc_ciphertext).to_vec()),
    }
}

/// The wire pool filter, as the domain names it.
///
/// Only this direction: the wire filter cannot say "no pools" (an empty set
/// means every shielded pool), so a round trip would gain pools.
pub fn pool_filter_from_wire(filter: &PoolTypeFilter) -> PoolFilter {
    let mut domain = PoolFilter::none();
    if filter.includes_transparent() {
        domain = domain.with_transparent();
    }
    for (pool, included) in [
        (ShieldedPool::Sapling, filter.includes_sapling()),
        (ShieldedPool::Orchard, filter.includes_orchard()),
        (ShieldedPool::Ironwood, filter.includes_ironwood()),
    ] {
        if included {
            domain = domain.with_pool(pool);
        }
    }
    domain
}

/// Appends `block`, read under `pools`, to `out` as an encoded
/// `CompactBlock` message: byte for byte what prost encodes from
/// [`compact_block_to_wire`].
pub fn write_compact_block(block: &CompactBlock, pools: PoolFilter, out: &mut Vec<u8>) {
    out.reserve(compact_block_len(block, pools));
    varint_field(2, u64::from(block.height), out);
    bytes_field(3, &<[u8; 32]>::from(block.hash), out);
    bytes_field(4, &<[u8; 32]>::from(block.prev_hash), out);
    varint_field(5, u64::from(block.time), out);
    for (index, tx) in served(block) {
        message_field(7, tx_len(index, tx), out);
        write_tx(index, tx, out);
    }
    let sizes = tree_sizes(&block.chain_metadata, pools);
    message_field(8, metadata_len(&sizes), out);
    for (tag, size) in (1..).zip(sizes) {
        varint_field(tag, u64::from(size), out);
    }
}

/// The encoded length of [`write_compact_block`]'s output.
pub fn compact_block_len(block: &CompactBlock, pools: PoolFilter) -> usize {
    let sizes = tree_sizes(&block.chain_metadata, pools);
    [
        varint_len(2, u64::from(block.height)),
        bytes_len(3, 32),
        bytes_len(4, 32),
        varint_len(5, u64::from(block.time)),
        served(block)
            .map(|(index, tx)| message_len(7, tx_len(index, tx)))
            .fold(0, usize::saturating_add),
        message_len(8, metadata_len(&sizes)),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

fn write_tx(index: usize, tx: &PreIndexCompactTx, out: &mut Vec<u8>) {
    varint_field(1, index as u64, out);
    bytes_field(2, &<[u8; 32]>::from(tx.txid), out);
    varint_field(3, u64::from(fee(tx.fee)), out);
    for nullifier in &tx.sapling_nullifiers {
        message_field(4, bytes_len(1, 32), out);
        bytes_field(1, &<[u8; 32]>::from(*nullifier), out);
    }
    for output in &tx.sapling_outputs {
        message_field(5, sapling_output_len(), out);
        write_sapling_output(output, out);
    }
    for action in &tx.orchard_actions {
        message_field(6, action_len(action), out);
        write_action(action, out);
    }
    for input in &tx.transparent_inputs {
        message_field(7, input_len(input), out);
        bytes_field(1, &<[u8; 32]>::from(input.prev_txid), out);
        varint_field(2, u64::from(input.prev_index), out);
    }
    for output in &tx.transparent_outputs {
        message_field(8, transparent_output_len(output), out);
        varint_field(1, u64::from(output.value), out);
        bytes_field(2, output.script.as_bytes(), out);
    }
    for action in &tx.ironwood_actions {
        message_field(9, action_len(action), out);
        write_action(action, out);
    }
}

fn tx_len(index: usize, tx: &PreIndexCompactTx) -> usize {
    let repeated = |tag: u32, lens: &mut dyn Iterator<Item = usize>| {
        lens.map(|len| message_len(tag, len))
            .fold(0, usize::saturating_add)
    };
    [
        varint_len(1, index as u64),
        bytes_len(2, 32),
        varint_len(3, u64::from(fee(tx.fee))),
        repeated(
            4,
            &mut tx.sapling_nullifiers.iter().map(|_| bytes_len(1, 32)),
        ),
        repeated(
            5,
            &mut tx.sapling_outputs.iter().map(|_| sapling_output_len()),
        ),
        repeated(6, &mut tx.orchard_actions.iter().map(action_len)),
        repeated(7, &mut tx.transparent_inputs.iter().map(input_len)),
        repeated(
            8,
            &mut tx.transparent_outputs.iter().map(transparent_output_len),
        ),
        repeated(9, &mut tx.ironwood_actions.iter().map(action_len)),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

fn write_sapling_output(output: &SaplingOutput, out: &mut Vec<u8>) {
    bytes_field(1, &<[u8; 32]>::from(output.cmu), out);
    bytes_field(2, &<[u8; 32]>::from(output.ephemeral_key), out);
    bytes_field(3, &<[u8; 52]>::from(output.enc_ciphertext), out);
}

fn sapling_output_len() -> usize {
    bytes_len(1, 32)
        .saturating_add(bytes_len(2, 32))
        .saturating_add(bytes_len(3, 52))
}

fn write_action(action: &CompactAction, out: &mut Vec<u8>) {
    bytes_field(1, &<[u8; 32]>::from(action.nullifier), out);
    if let Some(output) = &action.output {
        write_action_output(output, out);
    }
}

fn write_action_output(output: &ActionOutput, out: &mut Vec<u8>) {
    bytes_field(2, &<[u8; 32]>::from(output.cmx), out);
    bytes_field(3, &<[u8; 32]>::from(output.ephemeral_key), out);
    bytes_field(4, &<[u8; 52]>::from(output.enc_ciphertext), out);
}

fn action_len(action: &CompactAction) -> usize {
    let output = match action.output {
        Some(_) => bytes_len(2, 32)
            .saturating_add(bytes_len(3, 32))
            .saturating_add(bytes_len(4, 52)),
        None => 0,
    };
    bytes_len(1, 32).saturating_add(output)
}

fn input_len(input: &TransparentInput) -> usize {
    bytes_len(1, 32).saturating_add(varint_len(2, u64::from(input.prev_index)))
}

fn transparent_output_len(output: &TransparentOutput) -> usize {
    varint_len(1, u64::from(output.value))
        .saturating_add(bytes_len(2, output.script.as_bytes().len()))
}

fn metadata_len(sizes: &[u32; 3]) -> usize {
    (1..)
        .zip(sizes)
        .map(|(tag, size)| varint_len(tag, u64::from(*size)))
        .fold(0, usize::saturating_add)
}

/// The transactions the wire carries, each with its index in the block.
fn served(block: &CompactBlock) -> impl Iterator<Item = (usize, &PreIndexCompactTx)> {
    block
        .transactions
        .iter()
        .enumerate()
        .filter(|(_, tx)| has_pool_data(tx))
}

/// Whether a transaction has anything in a pool the block was read for,
/// outputs a nullifiers-only read left out included (lightwalletd parity).
fn has_pool_data(tx: &PreIndexCompactTx) -> bool {
    tx.sapling_outputs_unread > 0
        || !tx.transparent_inputs.is_empty()
        || !tx.transparent_outputs.is_empty()
        || !tx.sapling_nullifiers.is_empty()
        || !tx.sapling_outputs.is_empty()
        || !tx.orchard_actions.is_empty()
        || !tx.ironwood_actions.is_empty()
}

/// Sapling, Orchard and Ironwood tree sizes as the wire carries them.
fn tree_sizes(metadata: &ChainMetadata, pools: PoolFilter) -> [u32; 3] {
    match pools.includes_outputs() {
        true => [
            u32::from(metadata.sapling_tree_size),
            u32::from(metadata.orchard_tree_size),
            u32::from(metadata.ironwood_tree_size),
        ],
        false => [0; 3],
    }
}

/// The fee saturated to the wire's `u32` (D-60); 0, proto3's unset, where
/// unknown.
fn fee(fee: Option<Zatoshis>) -> u32 {
    fee.map_or(0, |fee| u32::try_from(u64::from(fee)).unwrap_or(u32::MAX))
}

fn varint_field(tag: u32, value: u64, out: &mut Vec<u8>) {
    if value != 0 {
        encode_key(tag, WireType::Varint, out);
        encode_varint(value, out);
    }
}

fn varint_len(tag: u32, value: u64) -> usize {
    match value {
        0 => 0,
        _ => key_len(tag).saturating_add(encoded_len_varint(value)),
    }
}

fn bytes_field(tag: u32, bytes: &[u8], out: &mut Vec<u8>) {
    if !bytes.is_empty() {
        message_field(tag, bytes.len(), out);
        out.extend_from_slice(bytes);
    }
}

fn bytes_len(tag: u32, len: usize) -> usize {
    match len {
        0 => 0,
        _ => message_len(tag, len),
    }
}

/// The key and length of a length-delimited field; its body follows.
fn message_field(tag: u32, len: usize, out: &mut Vec<u8>) {
    encode_key(tag, WireType::LengthDelimited, out);
    encode_varint(len as u64, out);
}

fn message_len(tag: u32, len: usize) -> usize {
    key_len(tag)
        .saturating_add(encoded_len_varint(len as u64))
        .saturating_add(len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use prost::Message as _;
    use zaino_primitives::types::{
        BlockHash, CompactCiphertext, CompactDifficulty, EphemeralKey, NoteCommitment, Nullifier,
        Script, TransactionId, TreeSize,
    };

    const REGTEST_BITS: u32 = 0x200f_0f0f;

    fn bytes32() -> impl Strategy<Value = [u8; 32]> {
        any::<[u8; 32]>()
    }

    fn zatoshis() -> impl Strategy<Value = Zatoshis> {
        (0..=Zatoshis::MAX.as_u64()).prop_map(|value| Zatoshis::new(value).expect("in range"))
    }

    fn output() -> impl Strategy<Value = (NoteCommitment, EphemeralKey, CompactCiphertext)> {
        (
            bytes32(),
            bytes32(),
            proptest::collection::vec(any::<u8>(), 52),
        )
            .prop_map(|(cmx, key, ciphertext)| {
                (
                    NoteCommitment::from(cmx),
                    EphemeralKey::from(key),
                    CompactCiphertext::try_new(&ciphertext).expect("52 bytes"),
                )
            })
    }

    fn action() -> impl Strategy<Value = CompactAction> {
        (bytes32(), proptest::option::of(output())).prop_map(|(nullifier, output)| CompactAction {
            nullifier: Nullifier::from(nullifier),
            output: output.map(|(cmx, ephemeral_key, enc_ciphertext)| ActionOutput {
                cmx,
                ephemeral_key,
                enc_ciphertext,
            }),
        })
    }

    fn tx() -> impl Strategy<Value = PreIndexCompactTx> {
        let few = 0..3usize;
        (
            bytes32(),
            proptest::collection::vec((bytes32(), any::<u32>()), few.clone()),
            proptest::collection::vec(
                (zatoshis(), proptest::collection::vec(any::<u8>(), 0..30)),
                few.clone(),
            ),
            proptest::collection::vec(bytes32(), few.clone()),
            proptest::collection::vec(output(), few.clone()),
            proptest::collection::vec(action(), few.clone()),
            proptest::collection::vec(action(), few),
            proptest::option::of(zatoshis()),
            0u16..3,
        )
            .prop_map(
                |(txid, inputs, outputs, nullifiers, sapling, orchard, ironwood, fee, unread)| {
                    PreIndexCompactTx {
                        sapling_outputs_unread: unread,
                        txid: TransactionId::from(txid),
                        transparent_inputs: inputs
                            .into_iter()
                            .map(|(prev_txid, prev_index)| TransparentInput {
                                prev_txid: TransactionId::from(prev_txid),
                                prev_index,
                            })
                            .collect(),
                        transparent_outputs: outputs
                            .into_iter()
                            .map(|(value, script)| TransparentOutput {
                                value,
                                script: Script::new(script),
                            })
                            .collect(),
                        sapling_nullifiers: nullifiers.into_iter().map(Nullifier::from).collect(),
                        sapling_outputs: sapling
                            .into_iter()
                            .map(|(cmu, ephemeral_key, enc_ciphertext)| SaplingOutput {
                                cmu,
                                ephemeral_key,
                                enc_ciphertext,
                            })
                            .collect(),
                        orchard_actions: orchard,
                        ironwood_actions: ironwood,
                        fee,
                    }
                },
            )
    }

    fn block() -> impl Strategy<Value = CompactBlock> {
        (
            bytes32(),
            bytes32(),
            any::<u32>(),
            any::<u32>(),
            proptest::collection::vec(tx(), 0..4),
            any::<[u32; 3]>(),
        )
            .prop_map(|(hash, prev_hash, height, time, transactions, sizes)| {
                CompactBlock {
                    hash: BlockHash::from(hash),
                    prev_hash: BlockHash::from(prev_hash),
                    height,
                    time,
                    bits: CompactDifficulty::try_from_bits(REGTEST_BITS).expect("valid bits"),
                    transactions,
                    chain_metadata: metadata(sizes),
                }
            })
    }

    fn pools() -> impl Strategy<Value = PoolFilter> {
        (any::<bool>(), any::<[bool; 3]>(), any::<bool>()).prop_map(
            |(transparent, shielded, nullifiers_only)| {
                let mut pools = PoolFilter::none();
                if transparent {
                    pools = pools.with_transparent();
                }
                for (pool, included) in [
                    ShieldedPool::Sapling,
                    ShieldedPool::Orchard,
                    ShieldedPool::Ironwood,
                ]
                .into_iter()
                .zip(shielded)
                {
                    if included {
                        pools = pools.with_pool(pool);
                    }
                }
                match nullifiers_only {
                    true => pools.nullifiers_only(),
                    false => pools,
                }
            },
        )
    }

    fn metadata([sapling, orchard, ironwood]: [u32; 3]) -> ChainMetadata {
        ChainMetadata {
            sapling_tree_size: TreeSize::from(sapling),
            orchard_tree_size: TreeSize::from(orchard),
            ironwood_tree_size: TreeSize::from(ironwood),
        }
    }

    fn written(block: &CompactBlock, pools: PoolFilter) -> Vec<u8> {
        let mut out = Vec::new();
        write_compact_block(block, pools, &mut out);
        out
    }

    proptest! {
        /// The writer's bytes are prost's encoding of the conversion, and its
        /// length is what it writes.
        #[test]
        fn the_writer_encodes_as_prost(block in block(), pools in pools()) {
            let bytes = written(&block, pools);
            prop_assert_eq!(&bytes, &compact_block_to_wire(&block, pools).encode_to_vec());
            prop_assert_eq!(compact_block_len(&block, pools), bytes.len());
        }
    }

    fn tx_with(fee: Option<u64>, nullifiers: usize) -> PreIndexCompactTx {
        PreIndexCompactTx {
            txid: TransactionId::from([7; 32]),
            transparent_inputs: Vec::new(),
            transparent_outputs: Vec::new(),
            sapling_nullifiers: vec![Nullifier::from([9; 32]); nullifiers],
            sapling_outputs: Vec::new(),
            orchard_actions: Vec::new(),
            ironwood_actions: Vec::new(),
            fee: fee.map(|fee| Zatoshis::new(fee).expect("in range")),
            sapling_outputs_unread: 0,
        }
    }

    fn block_of(transactions: Vec<PreIndexCompactTx>) -> CompactBlock {
        CompactBlock {
            hash: BlockHash::from([1; 32]),
            prev_hash: BlockHash::from([2; 32]),
            height: 1_000,
            time: 1_700_000_000,
            bits: CompactDifficulty::try_from_bits(REGTEST_BITS).expect("valid bits"),
            transactions,
            chain_metadata: metadata([10, 20, 30]),
        }
    }

    fn decoded(block: &CompactBlock, pools: PoolFilter) -> proto::CompactBlock {
        proto::CompactBlock::decode(written(block, pools).as_slice()).expect("decodes")
    }

    /// Fees: the domain fee, saturated to `u32`; unknown is unset (0).
    #[test]
    fn fees_are_served_saturated_and_unknown_is_unset() {
        let over = u64::from(u32::MAX) + 1;
        let block = block_of(vec![
            tx_with(Some(1_000), 1),
            tx_with(Some(over), 1),
            tx_with(None, 1),
        ]);
        let fees: Vec<u32> = decoded(&block, PoolFilter::all())
            .vtx
            .iter()
            .map(|tx| tx.fee)
            .collect();
        assert_eq!(fees, vec![1_000, u32::MAX, 0]);
        let unknown = compact_tx_to_wire(2, &block.transactions[2]).encode_to_vec();
        assert!(
            !unknown.contains(&0x18),
            "field 3 (fee) is absent: {unknown:02x?}"
        );
    }

    /// A transaction with nothing in a requested pool is dropped; the others
    /// keep their indexes in the block.
    #[test]
    fn empty_transactions_are_dropped_and_indexes_kept() {
        let block = block_of(vec![
            tx_with(None, 0),
            tx_with(None, 1),
            tx_with(None, 0),
            tx_with(None, 2),
        ]);
        let indexes: Vec<u64> = decoded(&block, PoolFilter::all())
            .vtx
            .iter()
            .map(|tx| tx.index)
            .collect();
        assert_eq!(indexes, vec![1, 3]);
    }

    /// A transaction whose only data is outputs a nullifiers-only read left
    /// out is served empty at its index (lightwalletd parity).
    #[test]
    fn a_transaction_with_unread_outputs_is_served_empty() {
        let mut outputs_only = tx_with(Some(5), 0);
        outputs_only.sapling_outputs_unread = 2;
        let block = block_of(vec![tx_with(None, 0), outputs_only, tx_with(None, 0)]);
        let vtx = decoded(&block, PoolFilter::all().nullifiers_only()).vtx;
        assert_eq!(
            vtx,
            vec![proto::CompactTx {
                index: 1,
                txid: vec![7; 32],
                fee: 5,
                ..Default::default()
            }]
        );
    }

    /// Tree sizes are the block's, but zero under the nullifiers-only filter.
    #[test]
    fn tree_sizes_are_zero_only_for_nullifiers_only_reads() {
        let block = block_of(vec![tx_with(None, 1)]);
        let sizes = |pools| {
            let metadata = decoded(&block, pools).chain_metadata.expect("present");
            [
                metadata.sapling_commitment_tree_size,
                metadata.orchard_commitment_tree_size,
                metadata.ironwood_commitment_tree_size,
            ]
        };
        assert_eq!(sizes(PoolFilter::all()), [10, 20, 30]);
        assert_eq!(sizes(PoolFilter::all().nullifiers_only()), [0, 0, 0]);
    }

    /// An action without its output carries only its nullifier.
    #[test]
    fn an_action_without_its_output_carries_its_nullifier_alone() {
        let mut tx = tx_with(None, 0);
        tx.orchard_actions.push(CompactAction {
            nullifier: Nullifier::from([5; 32]),
            output: None,
        });
        let block = block_of(vec![tx]);
        let action = decoded(&block, PoolFilter::all().nullifiers_only()).vtx[0].actions[0].clone();
        assert_eq!(
            action,
            proto::CompactOrchardAction {
                nullifier: vec![5; 32],
                ..Default::default()
            }
        );
    }

    fn sample() -> PreIndexCompactTx {
        let action = |tag: u8, output: bool| CompactAction {
            nullifier: Nullifier::from([tag; 32]),
            output: output.then(|| ActionOutput {
                cmx: NoteCommitment::from([tag.wrapping_add(1); 32]),
                ephemeral_key: EphemeralKey::from([tag.wrapping_add(2); 32]),
                enc_ciphertext: CompactCiphertext::from([tag; 52]),
            }),
        };
        PreIndexCompactTx {
            txid: TransactionId::from([0xaa; 32]),
            transparent_inputs: vec![TransparentInput {
                prev_txid: TransactionId::from([0xbb; 32]),
                prev_index: 3,
            }],
            transparent_outputs: vec![TransparentOutput {
                value: Zatoshis::new(50_000).expect("in range"),
                script: Script::new(vec![0x76, 0xa9]),
            }],
            sapling_nullifiers: vec![Nullifier::from([0xcc; 32])],
            sapling_outputs: Vec::new(),
            orchard_actions: vec![action(1, true)],
            ironwood_actions: vec![action(2, true)],
            fee: Some(Zatoshis::new(1_000).expect("in range")),
            sapling_outputs_unread: 0,
        }
    }

    /// Byte strings are protocol order: reversing them would hand wallets
    /// txids that name nothing.
    #[test]
    fn identifiers_stay_in_protocol_order() {
        let wire = compact_tx_to_wire(0, &sample());
        assert_eq!(wire.txid, vec![0xaa; 32]);
        assert_eq!(wire.vin[0].prevout_txid, vec![0xbb; 32]);
        assert_eq!(wire.spends[0].nf, vec![0xcc; 32]);
    }

    #[test]
    fn index_is_the_given_position_and_fee_the_domain_fee() {
        let wire = compact_tx_to_wire(7, &sample());
        assert_eq!(wire.index, 7);
        assert_eq!(wire.fee, 1_000);
    }

    /// Ironwood actions share `CompactOrchardAction`'s shape but not its field.
    #[test]
    fn ironwood_actions_stay_in_their_own_field() {
        let wire = compact_tx_to_wire(0, &sample());
        assert_eq!(wire.actions.len(), 1);
        assert_eq!(wire.ironwood_actions.len(), 1);
        assert_eq!(wire.actions[0].nullifier, vec![1; 32]);
        assert_eq!(wire.actions[0].cmx, vec![2; 32]);
        assert_eq!(wire.ironwood_actions[0].nullifier, vec![2; 32]);
    }

    #[test]
    fn transparent_outputs_carry_value_and_script() {
        let wire = compact_tx_to_wire(0, &sample());
        assert_eq!(wire.vout[0].value, 50_000);
        assert_eq!(wire.vout[0].script_pub_key, vec![0x76, 0xa9]);
    }

    #[test]
    fn a_transaction_with_no_spends_emits_empty_lists() {
        let mut tx = sample();
        tx.transparent_inputs.clear();
        tx.sapling_nullifiers.clear();
        let wire = compact_tx_to_wire(0, &tx);
        assert!(wire.vin.is_empty());
        assert!(wire.spends.is_empty());
    }

    /// The wire filter maps pool for pool; an empty one is every shielded pool.
    #[test]
    fn the_wire_filter_maps_to_the_domain() {
        let shielded = [
            ShieldedPool::Sapling,
            ShieldedPool::Orchard,
            ShieldedPool::Ironwood,
        ]
        .into_iter()
        .fold(PoolFilter::none(), PoolFilter::with_pool);
        assert_eq!(pool_filter_from_wire(&PoolTypeFilter::default()), shielded);
    }
}
