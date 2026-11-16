//! Zcash address parsing and classification.
//!
//! Zaino serves two address-validation RPCs, `validateaddress` and the
//! deprecated `z_validateaddress`. Neither reads the chain: both are pure
//! functions of an address string and a network. This crate is where that
//! logic lives, so both the indexing library and the serving layer can reach
//! it without either owning it.
//!
//! # Why a separate crate
//!
//! The `librustzcash` address stack (`zcash_address`, `zcash_keys`,
//! `zcash_transparent`, `sapling-crypto`) is a dependency set no other Zaino
//! crate wants. It cannot go in `zaino-primitives`, whose whole dependency
//! list is `thiserror` — that minimalism is what lets every other crate depend
//! on it. It does not belong in `zaino-state` either, which is the legacy
//! stack rather than domain logic. So it is a
//! leaf: nothing in Zaino depends on it except the consumers of these two
//! RPCs.
//!
//! # No serialization
//!
//! The types here are domain types. They carry raw key material as bytes, not
//! hex, and they do not derive `Serialize`. The legacy-compatible JSON shapes
//! these RPCs return — field names, hex encoding, the `type` / `address_type`
//! duplication — are the serving layer's concern and live in `zaino-serve`.
//!
//! # Network parameterisation
//!
//! Entry points are generic over [`zcash_protocol::consensus::Parameters`]
//! rather than taking a concrete network type. `zebra_chain::parameters::Network`
//! implements it, so callers pass theirs directly and this crate needs no
//! dependency on Zebra.

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

mod classify;
mod sapling;
mod validated;

pub use classify::{validate_address, z_validate_address};
pub use sapling::sapling_key_bytes;
pub use validated::{ValidatedAddress, ZValidatedAddress, DEPRECATION_NOTICE};
