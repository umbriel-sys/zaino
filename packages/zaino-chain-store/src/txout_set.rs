//! Zaino's UTXO-set commitment, re-exported from
//! [`zaino_primitives::txout_commitment`], where it is defined once for every
//! store and the chain head.

pub use zaino_primitives::txout_commitment::{
    canonical_entry, canonical_entry_parts, commitment_address, entry_digest, entry_digest_parts,
    is_unspendable, script_type_tag, Delta, TxOutSetAccumulator, TxOutSetError, MAX_SCRIPT_SIZE,
    TXOUT_SET_DOMAIN_TAG, TXOUT_SET_ENTRY_LEN,
};
