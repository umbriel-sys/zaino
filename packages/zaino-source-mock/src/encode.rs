//! Consensus encodings of the builder's transactions and blocks, in the
//! format each height's upgrade requires: v1 before Overwinter, v3, v4 from
//! Sapling, v5 from NU5, v6 for Ironwood bundles.
//!
//! Field elements are canonical and curve points are generator multiples, so
//! zebra's parse accepts them; proofs, signatures and ciphertexts are filler
//! nothing parses.

use std::sync::OnceLock;

use group::{Group as _, GroupEncoding as _};

use crate::schedule::Upgrade;

const V3_GROUP_ID: u32 = 0x03C4_8270;
const V4_GROUP_ID: u32 = 0x892F_2085;
const V5_GROUP_ID: u32 = 0x26A7_270A;
const V6_GROUP_ID: u32 = 0xD884_B698;
const OVERWINTERED: u32 = 0x8000_0000;

const ENC_CIPHERTEXT_LEN: usize = 580;
const OUT_CIPHERTEXT_LEN: usize = 80;
const GROTH_PROOF_LEN: usize = 192;
const SIGNATURE_LEN: usize = 64;
/// Spends and outputs enabled.
const ORCHARD_FLAGS: u8 = 0b0000_0011;
/// A regtest Equihash solution's length.
const REGTEST_SOLUTION_LEN: usize = 36;

/// A transparent input.
pub(crate) struct Input {
    pub(crate) prev_txid: [u8; 32],
    pub(crate) prev_index: u32,
    pub(crate) script: Vec<u8>,
    pub(crate) sequence: u32,
}

/// A transparent output.
pub(crate) struct Output {
    pub(crate) value: u64,
    pub(crate) script: Vec<u8>,
}

/// A Sapling output or an Orchard-shaped action's note.
pub(crate) struct Note {
    pub(crate) cm: [u8; 32],
    /// Picks the curve points.
    pub(crate) index: usize,
}

/// An Orchard-shaped action.
pub(crate) struct Action {
    pub(crate) nullifier: [u8; 32],
    pub(crate) note: Note,
}

/// One synthetic transaction.
pub(crate) struct Tx {
    pub(crate) inputs: Vec<Input>,
    pub(crate) outputs: Vec<Output>,
    pub(crate) expiry: u32,
    pub(crate) sapling: Vec<Note>,
    pub(crate) orchard: Vec<Action>,
    pub(crate) ironwood: Vec<Action>,
    /// The Orchard bundle's anchor: a root of the Orchard tree.
    pub(crate) orchard_anchor: [u8; 32],
    /// The Ironwood bundle's anchor: a root of the Ironwood tree.
    pub(crate) ironwood_anchor: [u8; 32],
}

/// The header fields the builder chooses.
pub(crate) struct Header {
    pub(crate) prev_hash: [u8; 32],
    pub(crate) merkle_root: [u8; 32],
    pub(crate) block_commitments: [u8; 32],
    pub(crate) time: u32,
    pub(crate) bits: u32,
    pub(crate) nonce: [u8; 32],
}

/// `bytes` as a canonical Jubjub-base or Pallas-base element: below 2^254,
/// under both moduli.
pub(crate) fn field(mut bytes: [u8; 32]) -> [u8; 32] {
    if let Some(top) = bytes.last_mut() {
        *top &= 0x3f;
    }
    bytes
}

/// The BIP 34 coinbase script for `height` (genesis: the genesis script),
/// then `extra` as an extra nonce.
pub(crate) fn coinbase_script(height: u32, extra: u64) -> Vec<u8> {
    if height == 0 {
        return zaino_source_zcash_rpc::GENESIS_COINBASE_SCRIPT_SIG.to_vec();
    }
    let mut script = match u8::try_from(height) {
        Ok(small @ 1..=16) => vec![0x50_u8.saturating_add(small)],
        _ => {
            let mut number: Vec<u8> = height.to_le_bytes().to_vec();
            while number.last() == Some(&0) {
                number.pop();
            }
            if number.last().is_some_and(|msb| msb & 0x80 != 0) {
                number.push(0);
            }
            let mut push = vec![u8::try_from(number.len()).unwrap_or(5)];
            push.extend(number);
            push
        }
    };
    script.push(8);
    script.extend(extra.to_le_bytes());
    script
}

/// `tx` in the format `upgrade` requires.
pub(crate) fn transaction(tx: &Tx, upgrade: Option<Upgrade>) -> Vec<u8> {
    let mut w = Vec::new();
    match upgrade {
        None => {
            u32le(&mut w, 1);
            transparent(&mut w, tx);
            u32le(&mut w, 0);
        }
        Some(Upgrade::Overwinter) => {
            u32le(&mut w, OVERWINTERED | 3);
            u32le(&mut w, V3_GROUP_ID);
            transparent(&mut w, tx);
            u32le(&mut w, 0);
            u32le(&mut w, tx.expiry);
            compact_size(&mut w, 0); // JoinSplits
        }
        Some(Upgrade::Sapling | Upgrade::Blossom | Upgrade::Heartwood | Upgrade::Canopy) => {
            u32le(&mut w, OVERWINTERED | 4);
            u32le(&mut w, V4_GROUP_ID);
            transparent(&mut w, tx);
            u32le(&mut w, 0);
            u32le(&mut w, tx.expiry);
            w.extend(0_i64.to_le_bytes()); // valueBalanceSapling
            compact_size(&mut w, 0); // spends
            compact_size(&mut w, tx.sapling.len());
            for note in &tx.sapling {
                sapling_output(&mut w, note);
                filler(&mut w, GROTH_PROOF_LEN, 0x77);
            }
            compact_size(&mut w, 0); // JoinSplits
            if !tx.sapling.is_empty() {
                filler(&mut w, SIGNATURE_LEN, 0x55); // bindingSigSapling
            }
        }
        Some(upgrade) => {
            let v6 = !tx.ironwood.is_empty();
            match v6 {
                true => {
                    u32le(&mut w, OVERWINTERED | 6);
                    u32le(&mut w, V6_GROUP_ID);
                }
                false => {
                    u32le(&mut w, OVERWINTERED | 5);
                    u32le(&mut w, V5_GROUP_ID);
                }
            }
            u32le(&mut w, upgrade.branch_id());
            u32le(&mut w, 0);
            u32le(&mut w, tx.expiry);
            transparent(&mut w, tx);
            compact_size(&mut w, 0); // spends
            compact_size(&mut w, tx.sapling.len());
            for note in &tx.sapling {
                sapling_output(&mut w, note);
            }
            if !tx.sapling.is_empty() {
                w.extend(0_i64.to_le_bytes()); // valueBalanceSapling
            }
            filler(
                &mut w,
                GROTH_PROOF_LEN.saturating_mul(tx.sapling.len()),
                0x77,
            );
            if !tx.sapling.is_empty() {
                filler(&mut w, SIGNATURE_LEN, 0x55); // bindingSigSapling
            }
            orchard_bundle(&mut w, &tx.orchard, tx.orchard_anchor);
            if v6 {
                orchard_bundle(&mut w, &tx.ironwood, tx.ironwood_anchor);
            }
        }
    }
    w
}

/// A block header with a regtest solution.
pub(crate) fn header(header: &Header) -> Vec<u8> {
    let mut w = Vec::new();
    u32le(&mut w, 4);
    w.extend(header.prev_hash);
    w.extend(header.merkle_root);
    w.extend(header.block_commitments);
    u32le(&mut w, header.time);
    u32le(&mut w, header.bits);
    w.extend(header.nonce);
    compact_size(&mut w, REGTEST_SOLUTION_LEN);
    filler(&mut w, REGTEST_SOLUTION_LEN, 0);
    w
}

/// A block: its header, then its transactions.
pub(crate) fn block(header: &[u8], transactions: &[Vec<u8>]) -> Vec<u8> {
    let mut w = header.to_vec();
    compact_size(&mut w, transactions.len());
    for tx in transactions {
        w.extend(tx);
    }
    w
}

fn transparent(w: &mut Vec<u8>, tx: &Tx) {
    compact_size(w, tx.inputs.len());
    for input in &tx.inputs {
        w.extend(input.prev_txid);
        u32le(w, input.prev_index);
        var_bytes(w, &input.script);
        u32le(w, input.sequence);
    }
    compact_size(w, tx.outputs.len());
    for output in &tx.outputs {
        w.extend(output.value.to_le_bytes());
        var_bytes(w, &output.script);
    }
}

/// cv, cmu, epk, encCiphertext, outCiphertext.
fn sapling_output(w: &mut Vec<u8>, note: &Note) {
    w.extend(jubjub_point(note.index));
    w.extend(note.cm);
    w.extend(jubjub_point(note.index.wrapping_add(1)));
    filler(w, ENC_CIPHERTEXT_LEN, 0x5a);
    filler(w, OUT_CIPHERTEXT_LEN, 0x6b);
}

fn orchard_bundle(w: &mut Vec<u8>, actions: &[Action], anchor: [u8; 32]) {
    compact_size(w, actions.len());
    if actions.is_empty() {
        return;
    }
    for action in actions {
        let index = action.note.index;
        w.extend(pallas_point(index)); // cv
        w.extend(action.nullifier);
        w.extend(pallas_point(index.wrapping_add(1))); // rk
        w.extend(action.note.cm);
        w.extend(pallas_point(index.wrapping_add(2))); // epk
        filler(w, ENC_CIPHERTEXT_LEN, 0xa5);
        filler(w, OUT_CIPHERTEXT_LEN, 0xb6);
    }
    w.push(ORCHARD_FLAGS);
    w.extend(0_i64.to_le_bytes()); // valueBalance
    w.extend(anchor);
    let proof = halo2_proof_len(actions.len());
    compact_size(w, proof);
    filler(w, proof, 0x77);
    filler(w, SIGNATURE_LEN.saturating_mul(actions.len()), 0x55);
    filler(w, SIGNATURE_LEN, 0x55); // bindingSig
}

/// A Halo 2 proof's length for `actions` actions, as the orchard crate
/// sizes it.
pub(crate) fn halo2_proof_len(actions: usize) -> usize {
    2720_usize.saturating_add(2272_usize.saturating_mul(actions))
}

/// Points the builder cycles through.
const POINTS: usize = 16;

fn jubjub_point(index: usize) -> [u8; 32] {
    static POOL: OnceLock<Vec<[u8; 32]>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        (1..=POINTS as u64)
            .map(|k| (jubjub::SubgroupPoint::generator() * jubjub::Fr::from(k)).to_bytes())
            .collect()
    });
    pool.get(index % POINTS).copied().unwrap_or_default()
}

fn pallas_point(index: usize) -> [u8; 32] {
    static POOL: OnceLock<Vec<[u8; 32]>> = OnceLock::new();
    let pool = POOL.get_or_init(|| {
        (1..=POINTS as u64)
            .map(|k| {
                (pasta_curves::pallas::Point::generator() * pasta_curves::pallas::Scalar::from(k))
                    .to_bytes()
            })
            .collect()
    });
    pool.get(index % POINTS).copied().unwrap_or_default()
}

fn u32le(w: &mut Vec<u8>, value: u32) {
    w.extend(value.to_le_bytes());
}

fn compact_size(w: &mut Vec<u8>, n: usize) {
    let n = n as u64;
    match n {
        0..=0xfc => w.push(n as u8),
        0xfd..=0xffff => {
            w.push(0xfd);
            w.extend((n as u16).to_le_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            w.push(0xfe);
            w.extend((n as u32).to_le_bytes());
        }
        _ => {
            w.push(0xff);
            w.extend(n.to_le_bytes());
        }
    }
}

fn var_bytes(w: &mut Vec<u8>, bytes: &[u8]) {
    compact_size(w, bytes.len());
    w.extend(bytes);
}

fn filler(w: &mut Vec<u8>, len: usize, byte: u8) {
    w.resize(w.len().saturating_add(len), byte);
}
