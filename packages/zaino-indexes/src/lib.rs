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

mod layout;
mod schema;

pub use layout::{Field, FieldWidth, Layout, LayoutError};
pub use schema::{NamespaceLayout, Schema};
