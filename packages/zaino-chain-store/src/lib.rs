//! ChainStore: the finalised state of the chain.
//!
//! ChainStore owns everything below the reorg seam — the blocks a validator
//! can no longer reorganise away, and the indexes built over them. It is the
//! half of the chain that only ever grows.
//!
//! This crate is the domain half: vocabulary and ports, no runtime and no
//! storage. An implementation is a separate crate — `zaino-chain-store-zainodb`
//! is the LMDB one — so a consumer can name what a store answers without
//! depending on the machinery that answers it, and so a second implementation
//! is a matter of satisfying these traits rather than of replacing a crate
//! everything else names.
//!
//! # Capabilities, not a single surface
//!
//! Only [`ChainStoreReader`] is universal. Everything beyond it is an index a
//! deployment may or may not build — compact blocks, transaction positions,
//! spent outputs, address history — and each is its own trait, so a consumer's
//! bound names exactly what it uses and a store that cannot serve something
//! simply does not implement it. Absence is then a compile-time fact where it
//! can be, and a runtime one ([`StoreCapabilities`]) where it cannot, because
//! a store on an older schema genuinely lacks indexes until it has migrated.
//!
//! # What ChainStore is not
//!
//! It is not the chain. It answers for heights up to its watermark and nothing
//! above, so a read past that is [`ChainStoreError::AboveWatermark`] and not a
//! miss — the block very likely exists, in the chain head. Complete answers
//! combine the two, and that combining happens above both.
//!
//! It does not hold blocks. What it stores is a projection: the fields an
//! index reads, not the bytes a block hash commits to. Raw blocks and raw
//! transactions come from the validator, and no port here offers them, so that
//! a consumer cannot mistake the store for a source of consensus data.
//!
//! It does not handle reorgs as a read concern. Its history is append-only in
//! normal operation; [`ChainStoreIngest::rewind_to`] exists as a repair path,
//! not as part of following the chain.
//!
//! [`txout_set`] re-exports Zaino's UTXO-set commitment from
//! `zaino_primitives::txout_commitment`, its one definition: every store and
//! the chain head must compute the identical digest.

// production code never panics: no indexing, unchecked arithmetic or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::let_underscore_must_use,
        let_underscore_drop
    )
)]

pub mod block;
pub mod capability;
pub mod config;
pub mod error;
pub mod output;
pub mod ports;
#[cfg(feature = "transparent_address_history_experimental")]
pub mod transparent;
pub mod txout_set;

pub use block::{FrozenBlock, PoolFilter, StoredBlock, StoredTx};
pub use capability::{
    MigrationState, Provenance, SchemaVersion, StoreCapabilities, StoreCapability, StoreSchema,
    StoreWatermark,
};
pub use config::ChainStoreConfig;
pub use error::{ChainStoreError, ChainStoreSourceError};
pub use output::{SpenderRef, StoredTxOut};
pub use ports::{
    ChainStoreFreezeSink, ChainStoreIngest, ChainStoreReader, ChainStoreReaderCapability,
    ChainStoreService, ChainStoreSource, CompactBlockRead, CompactBlockReadCapability,
    SpentOutputIndex, SpentOutputIndexCapability, StoredBlockRead, StoredBlockReadCapability,
    TransactionIndex, TransactionIndexCapability, TxOutSetIndex, TxOutSetIndexCapability,
};
pub use txout_set::{
    canonical_entry, canonical_entry_parts, commitment_address, entry_digest, entry_digest_parts,
    is_legacy_unspendable, is_unspendable, script_type_tag, Delta, TxOutSetAccumulator,
    TxOutSetError, MAX_SCRIPT_SIZE, TXOUT_SET_DOMAIN_TAG, TXOUT_SET_ENTRY_LEN,
};

#[cfg(feature = "transparent_address_history_experimental")]
pub use ports::{TransparentHistoryIndex, TransparentHistoryIndexCapability};
#[cfg(feature = "transparent_address_history_experimental")]
pub use transparent::{LocatedOutput, LocatedSpend, StoreAddressEffects, TransparentHistoryQuery};
