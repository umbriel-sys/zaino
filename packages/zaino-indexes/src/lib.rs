#![doc = include_str!("../usage.md")]
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

pub mod headers;
mod index;
mod layout;
mod schema;
#[cfg(test)]
mod testing;

pub use index::{Fold, FoldError, Index};
pub use layout::{Field, FieldWidth, Layout, LayoutError};
pub use schema::{NamespaceLayout, Schema};
