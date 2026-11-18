//! Composite Zebra source.
//!
//! Reaching a Zebra validator involves two transports with different reach.
//! Reading its state database directly is far faster but answers only what the
//! finalized state holds; the JSON-RPC interface answers everything but pays
//! request/response cost for each call.
//!
//! The previous implementation expressed this as an enum whose every method
//! matched on the arm, which conflated two separate questions: *can* this
//! transport answer, and *should* it when both can. Here the two are separate
//! mechanisms:
//!
//! - **Capability is structural.** `ZebraReadStateAdapter` simply does not
//!   implement the traits it cannot serve — the node passthroughs, the mempool,
//!   the derived delta queries. There is no arm to get wrong, and a
//!   deployment's transport mix cannot silently change what a query means.
//! - **Preference is this routing table.** Each trait below delegates: to the
//!   state service where it can answer, to JSON-RPC otherwise, and — in the two
//!   cases where the fast path is *semantically* incomplete rather than merely
//!   absent — to the state service first and JSON-RPC after.
//!
//! The JSON-RPC adapter is not optional. Every deployment has one, because the
//! mempool and the passthrough RPCs are reachable no other way. The state
//! adapter is the accelerator layered over it, which is why it is the `Option`.
//!
//! This crate is everything zebra-specific: the read-state adapter over
//! `zebra-state`, the quirk table that reads what zebrad answers differently
//! from the zcash RPC spec, and the composite. The JSON-RPC adapter itself
//! (`zaino-source-zcash-rpc`) names no validator.

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

mod fallback;
mod quirks;
mod read_state;
mod routing;

pub use quirks::rpc_adapter;
pub use read_state::ZebraReadStateAdapter;
pub use routing::ZebraValidator;
