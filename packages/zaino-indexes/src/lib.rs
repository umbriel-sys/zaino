#![doc = include_str!("../usage.md")]
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

pub mod address_history;
mod block;
pub mod chain_metadata;
mod codec;
pub mod compact_block;
pub mod headers;
pub mod history_tree;
mod id;
mod index;
mod layout;
mod legacy_tree;
mod schema;
pub mod spent;
#[cfg(test)]
mod testing;
pub mod transparent;
pub mod tree_state;
pub mod tx_out_set;
pub mod txid_location;
pub mod txids;

pub use block::{
    CompletedSubtree, FoldBlock, FoldBlockError, FoldBlockParts, FoldTx, NoteFrontier,
    PoolFrontiers,
};
pub use id::{IndexId, IndexState};
pub use index::{Fold, FoldError, Index};
pub use layout::{Field, FieldWidth, Layout, LayoutError};
pub use legacy_tree::{frontier_from_legacy_tree, LegacyTreeError};
pub use schema::{NamespaceLayout, Schema};
pub use tree_state::PoolFrontier;
