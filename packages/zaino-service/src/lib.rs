//! One configurable indexer service: ChainView over the chain head and the
//! store, the coherent mempool, and validator passthrough, with one manifest
//! saying who answers each capability.

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

mod block_time;
mod capability;
mod config;
mod engine;
mod error;
mod hashes;
mod history;
mod mempool_reads;
mod reads;
mod schedule;
mod source;

pub use capability::{Capability, Provision, ServiceManifest};
pub use config::{DiskStore, Mode, ServiceConfig, Store};
pub use engine::{Service, SpawnError, Statuses};
pub use error::ServiceError;
pub use hashes::BlockHashAt;
pub use history::{HistoryNode, HistoryProof};
pub use mempool_reads::{Placement, Spend, TransactionStatus};
pub use reads::{ChainSnapshot, ServiceSnapshot};
pub use schedule::{schedule_at, ScheduleAt};
pub use source::ServiceSource;
