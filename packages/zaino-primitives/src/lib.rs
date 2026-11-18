//! Zaino primitives — vocabulary types for the Zcash chain.
//!
//! Zero-dependency crate. All Zaino crates that need chain-level types
//! (heights, hashes) depend on this crate instead of on each other. The
//! `zebra` feature adds the [`zebra`] conversions from `zebra_chain` types.

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

#[cfg(feature = "txout-commitment")]
pub mod txout_commitment;
pub mod types;
#[cfg(feature = "zebra")]
pub mod zebra;
