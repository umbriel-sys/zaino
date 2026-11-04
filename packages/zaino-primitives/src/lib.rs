//! Zaino primitives — vocabulary types for the Zcash chain.
//!
//! Zero-dependency crate. All Zaino crates that need chain-level types
//! (heights, hashes) depend on this crate instead of on each other. The
//! `zebra` feature adds the [`zebra`] conversions from `zebra_chain` types.

pub mod types;
#[cfg(feature = "zebra")]
pub mod zebra;
