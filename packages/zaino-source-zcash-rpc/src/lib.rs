//! Zcash JSON-RPC adapter — implements zaino-source query traits
//! against any validator serving the zcash RPC interface.
//!
//! Uses [`zaino_rpc::RpcClient`] for transport and the spec's response
//! shapes for deserialization. What the spec leaves to the node (a result
//! cap's wording) is read through a [`Quirks`] table the caller supplies.

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

mod adapter;
mod parse;
mod raw_block;

pub use adapter::{Quirks, ZcashRpcAdapter};
pub use raw_block::{parse_block, BlockVerification, RawBlockError, GENESIS_COINBASE_SCRIPT_SIG};

/// A verbose `getblock` transaction object the adapter could not read.
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct TransactionObjectError(parse::ParseError);

/// Reads a verbose `getblock` transaction object exactly as the adapter does.
pub fn parse_transaction_object(
    value: &serde_json::Value,
) -> Result<zaino_primitives::types::rpc::TransactionVerbose, TransactionObjectError> {
    parse::parse_transaction_verbose(value).map_err(TransactionObjectError)
}

/// Reads a `scriptPubKey` object exactly as the adapter does.
pub fn parse_script_pub_key_object(
    value: &serde_json::Value,
) -> Result<zaino_primitives::types::rpc::ScriptPubKey, TransactionObjectError> {
    parse::parse_script_pub_key(value).map_err(TransactionObjectError)
}
