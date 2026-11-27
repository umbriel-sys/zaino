//! Live-test harness.
//!
//! Two ports and their implementations: [`Validator`] (a `zebrad` subprocess,
//! [`Zebrad`]) and [`Wallet`] (an in-process librustzcash wallet,
//! [`LrzWallet`]), plus the in-process indexer launcher ([`Zainod`]) and the
//! fixture cache ([`ChainCache`]) every test starts from. The remaining
//! modules are the small helpers the tests share: JSON-RPC parity assertions
//! (`rpc`/`json`), hex conversion across the JSON-RPC / gRPC boundary
//! (`hex`), the finalised-index gate (`finalised`) and the independent second
//! transaction parser (`legacy_parser`).
//!
//! `zebrad` is found on `PATH`, or at `ZEBRAD_BIN`.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![allow(async_fn_in_trait)]

pub mod legacy_parser;

pub mod cache;
pub mod finalised;
pub mod hex;
pub mod indexer;
pub mod json;
pub mod librustzcash;
pub mod process;
pub mod rpc;
pub mod validator;
pub mod wallet;
pub mod zebrad;

pub use cache::{ChainCache, Event, Fixture, Manifest, Party};
pub use finalised::wait_for_finalised;
pub use indexer::{Backend, IndexerConfig, Zainod};
pub use json::{
    assert_json_equal_ignoring, assert_json_shape_matches, json_equal_ignoring, json_shape_matches,
    sort_json_array,
};
pub use librustzcash::LrzWallet;
pub use rpc::{assert_rpc_parity, JsonRpcClient};
pub use validator::{
    Network, Pool, Validator, FAUCET_SEED, FILLER_ADDRESS, ORCHARD_MINER_ADDRESS, RECIPIENT_SEED,
    REGTEST_ACTIVATION_HEIGHTS, SAPLING_MINER_ADDRESS, TRANSPARENT_MINER_ADDRESS,
};
pub use wallet::{PoolBalances, Wallet};
pub use zebrad::{Zebrad, ZebradConfig, ZEBRAD_MIN_VERSION};

pub use zaino_primitives::types::ActivationHeights;
pub use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
pub use zaino_proto::proto::service::ShieldedProtocol;
pub use zcash_protocol::TxId;
