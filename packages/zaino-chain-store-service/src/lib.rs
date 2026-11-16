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

mod assemble;
mod config;
mod ingest;
mod linked;
mod metadata;
mod record;
mod set;
mod store;
#[cfg(test)]
mod tests;

pub use config::{ChainStoreServiceConfig, IndexesConfig, IngestConfig, OnCorruption};
pub use linked::{LinkedChainStore, LinkedError, Polling, PrimaryStatus};
pub use record::{PrimaryState, RecordError};
pub use set::{IndexSet, NamedSet, SetError};
pub use store::{BootError, ChainStore, Serviceability};
