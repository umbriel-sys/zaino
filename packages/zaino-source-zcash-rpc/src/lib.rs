//! Zcash JSON-RPC adapter — implements zaino-source query traits
//! against any validator serving the zcash RPC interface.
//!
//! Uses [`zaino_rpc::RpcClient`] for transport and the spec's response
//! shapes for deserialization. What the spec leaves to the node (a result
//! cap's wording) is read through a [`Quirks`] table the caller supplies.

mod adapter;
mod parse;
mod raw_block;

pub use adapter::{Quirks, ZcashRpcAdapter};
pub use raw_block::{
    merkle_root, parse_block, BlockVerification, RawBlockError, GENESIS_COINBASE_SCRIPT_SIG,
};
