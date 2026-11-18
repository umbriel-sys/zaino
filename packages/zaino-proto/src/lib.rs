//! Holds tonic generated code for the lightwallet service RPCs and compact formats.
//!
//! * We plan to eventually rely on LibRustZcash's versions but hold our our here for development purposes.
//! * Currently only holds the lightwallet proto files.

#![forbid(unsafe_code)]
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

pub mod proto;

#[cfg(feature = "grpc_proxy_server")]
pub use prost;
#[cfg(feature = "grpc_proxy_server")]
pub use tonic;
