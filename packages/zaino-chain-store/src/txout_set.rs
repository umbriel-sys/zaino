//! Zaino's UTXO-set commitment, re-exported from
//! [`zaino_primitives::txout_commitment`], where it is defined once for every
//! store and the chain head.

use zaino_primitives::types::{ScriptType, TransparentAddressKey};

pub use zaino_primitives::txout_commitment::{
    canonical_entry, canonical_entry_parts, commitment_address, entry_digest, entry_digest_parts,
    is_unspendable, script_type_tag, Delta, TxOutSetAccumulator, TxOutSetError, MAX_SCRIPT_SIZE,
    TXOUT_SET_DOMAIN_TAG, TXOUT_SET_ENTRY_LEN,
};

/// The zainodb-era set rule, for stores that keep an output's address key
/// but not its script: P2PKH and P2SH only. Superseded by
/// [`is_unspendable`] (zcashd's rule over the script); removed with zainodb
/// in Stage 6.
pub fn is_legacy_unspendable(address: &TransparentAddressKey) -> bool {
    !matches!(address.script_type, ScriptType::P2PKH | ScriptType::P2SH)
}
