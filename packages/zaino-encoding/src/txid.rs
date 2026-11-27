//! ZIP 244 txid digests for v5 and v6 transactions (v6 per ZIP 229).

use blake2b_simd::{Params, State};

use super::{RawOrchard, RawSapling, RawTx, TxVersion, Zip244Header};

const TX_PREFIX: &[u8; 12] = b"ZcashTxHash_";
const HEADERS: &[u8; 16] = b"ZTxIdHeadersHash";
const TRANSPARENT: &[u8; 16] = b"ZTxIdTranspaHash";
const PREVOUTS: &[u8; 16] = b"ZTxIdPrevoutHash";
const SEQUENCE: &[u8; 16] = b"ZTxIdSequencHash";
const OUTPUTS: &[u8; 16] = b"ZTxIdOutputsHash";
const SAPLING: &[u8; 16] = b"ZTxIdSaplingHash";
const SAPLING_SPENDS: &[u8; 16] = b"ZTxIdSSpendsHash";
const SAPLING_SPENDS_COMPACT: &[u8; 16] = b"ZTxIdSSpendCHash";
const SAPLING_SPENDS_NONCOMPACT_V5: &[u8; 16] = b"ZTxIdSSpendNHash";
const SAPLING_SPENDS_NONCOMPACT_V6: &[u8; 16] = b"ZTxIdSSpendNH_v6";
const SAPLING_OUTPUTS: &[u8; 16] = b"ZTxIdSOutputHash";
const SAPLING_OUTPUTS_COMPACT: &[u8; 16] = b"ZTxIdSOutC__Hash";
const SAPLING_OUTPUTS_MEMOS: &[u8; 16] = b"ZTxIdSOutM__Hash";
const SAPLING_OUTPUTS_NONCOMPACT: &[u8; 16] = b"ZTxIdSOutN__Hash";

/// Personalisations of one Orchard-shaped bundle digest format.
struct BundleFormat {
    bundle: &'static [u8; 16],
    compact: &'static [u8; 16],
    memos: &'static [u8; 16],
    noncompact: &'static [u8; 16],
    /// Only the v5 Orchard digest commits to the anchor.
    includes_anchor: bool,
}

const ORCHARD_V5: BundleFormat = BundleFormat {
    bundle: b"ZTxIdOrchardHash",
    compact: b"ZTxIdOrcActCHash",
    memos: b"ZTxIdOrcActMHash",
    noncompact: b"ZTxIdOrcActNHash",
    includes_anchor: true,
};

const ORCHARD_V6: BundleFormat = BundleFormat {
    bundle: b"ZTxIdOrchardH_v6",
    compact: b"ZTxIdOrcActCHash",
    memos: b"ZTxIdOrcActMHash",
    noncompact: b"ZTxIdOrcActNHash",
    includes_anchor: false,
};

const IRONWOOD_V6: BundleFormat = BundleFormat {
    bundle: b"ZTxIdIronwd_H_v6",
    compact: b"ZTxIdIrnActCH_v6",
    memos: b"ZTxIdIrnActMH_v6",
    noncompact: b"ZTxIdIrnActNH_v6",
    includes_anchor: false,
};

/// The txid of a v5 or v6 transaction.
pub(super) fn zip244(tx: &RawTx<'_>, header: &Zip244Header<'_>) -> [u8; 32] {
    let v6 = tx.version == TxVersion::V6;
    let mut personal = [0u8; 16];
    for (dst, src) in personal
        .iter_mut()
        .zip(TX_PREFIX.iter().chain(header.branch_id.iter()))
    {
        *dst = *src;
    }
    let mut h = hasher(&personal);
    h.update(&digest(hasher(HEADERS).update(header.fields)));
    h.update(&transparent_digest(tx));
    h.update(&sapling_digest(&tx.sapling, v6));
    if v6 {
        h.update(&bundle_digest(tx.orchard.as_ref(), &ORCHARD_V6));
        h.update(&bundle_digest(tx.ironwood.as_ref(), &IRONWOOD_V6));
    } else {
        h.update(&bundle_digest(tx.orchard.as_ref(), &ORCHARD_V5));
    }
    digest(&h)
}

fn hasher(personal: &[u8; 16]) -> State {
    Params::new().hash_length(32).personal(personal).to_state()
}

fn digest(state: &State) -> [u8; 32] {
    let hash = state.finalize();
    // The hash length is fixed at 32 by `hasher`.
    core::array::from_fn(|i| hash.as_bytes().get(i).copied().unwrap_or_default())
}

fn transparent_digest(tx: &RawTx<'_>) -> [u8; 32] {
    let mut h = hasher(TRANSPARENT);
    if !(tx.inputs.is_empty() && tx.outputs.is_empty()) {
        let mut prevouts = hasher(PREVOUTS);
        let mut sequence = hasher(SEQUENCE);
        for input in &tx.inputs {
            prevouts
                .update(&input.prev_txid)
                .update(&input.prev_index.to_le_bytes());
            sequence.update(&input.sequence.to_le_bytes());
        }
        let mut outputs = hasher(OUTPUTS);
        for output in &tx.outputs {
            outputs.update(output.raw);
        }
        h.update(&digest(&prevouts))
            .update(&digest(&sequence))
            .update(&digest(&outputs));
    }
    digest(&h)
}

fn sapling_digest(sapling: &RawSapling<'_>, v6: bool) -> [u8; 32] {
    let mut h = hasher(SAPLING);
    if !sapling.is_empty() {
        let mut spends = hasher(SAPLING_SPENDS);
        if !sapling.spends.is_empty() {
            let mut compact = hasher(SAPLING_SPENDS_COMPACT);
            let (mut noncompact, anchor) = if v6 {
                (hasher(SAPLING_SPENDS_NONCOMPACT_V6), None)
            } else {
                (hasher(SAPLING_SPENDS_NONCOMPACT_V5), sapling.anchor)
            };
            for spend in &sapling.spends {
                compact.update(&spend.nullifier);
                noncompact.update(spend.cv);
                if let Some(anchor) = &anchor {
                    noncompact.update(anchor);
                }
                noncompact.update(spend.rk);
            }
            spends
                .update(&digest(&compact))
                .update(&digest(&noncompact));
        }
        let mut outputs = hasher(SAPLING_OUTPUTS);
        if !sapling.outputs.is_empty() {
            let mut compact = hasher(SAPLING_OUTPUTS_COMPACT);
            let mut memos = hasher(SAPLING_OUTPUTS_MEMOS);
            let mut noncompact = hasher(SAPLING_OUTPUTS_NONCOMPACT);
            for output in &sapling.outputs {
                let note = &output.note;
                compact
                    .update(&note.cm)
                    .update(&note.epk)
                    .update(&note.enc_compact);
                memos.update(note.enc_memo);
                noncompact
                    .update(output.cv)
                    .update(note.enc_tail)
                    .update(note.out);
            }
            outputs
                .update(&digest(&compact))
                .update(&digest(&memos))
                .update(&digest(&noncompact));
        }
        h.update(&digest(&spends))
            .update(&digest(&outputs))
            .update(&sapling.value_balance.to_le_bytes());
    }
    digest(&h)
}

fn bundle_digest(bundle: Option<&RawOrchard<'_>>, format: &BundleFormat) -> [u8; 32] {
    let mut h = hasher(format.bundle);
    if let Some(bundle) = bundle {
        let mut compact = hasher(format.compact);
        let mut memos = hasher(format.memos);
        let mut noncompact = hasher(format.noncompact);
        for action in &bundle.actions {
            let note = &action.note;
            compact
                .update(&action.nullifier)
                .update(&note.cm)
                .update(&note.epk)
                .update(&note.enc_compact);
            memos.update(note.enc_memo);
            noncompact
                .update(action.cv)
                .update(action.rk)
                .update(note.enc_tail)
                .update(note.out);
        }
        h.update(&digest(&compact))
            .update(&digest(&memos))
            .update(&digest(&noncompact))
            .update(&[bundle.flags])
            .update(&bundle.value_balance.to_le_bytes());
        if format.includes_anchor {
            h.update(&bundle.anchor);
        }
    }
    digest(&h)
}
