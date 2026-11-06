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

mod block;
pub mod headers;
mod index;
mod layout;
mod schema;
#[cfg(test)]
mod testing;

pub use block::{FoldBlock, FoldBlockError, FoldTx};
pub use index::{Fold, FoldError, Index};
pub use layout::{Field, FieldWidth, Layout, LayoutError};
pub use schema::{NamespaceLayout, Schema};
