//! Holds gRPC and JSON RPC servers capable of servicing clients over TCP.
//!
//! - server::ingestor has been built so that other ingestors may be added that use different transport protocols (Nym, TOR).
//!
//! Also holds rust implementations of the LightWallet gRPC Service (CompactTxStreamerServer).

#![warn(missing_docs)]
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

pub mod rpc;
pub mod server;

/// Prometheus metric names emitted by this crate; the single source of truth shared with `zainod`'s `describe_*` registrations (which carry the descriptions).
#[allow(missing_docs)] // names are self-describing; descriptions live in zainod
pub mod metric_names {
    // `_count` = request volume, so neither surface has a request counter
    pub const GRPC_REQUEST_DURATION_SECONDS: &str = "zaino.grpc.request_duration_seconds";
    pub const JSONRPC_REQUEST_DURATION_SECONDS: &str = "zaino.jsonrpc.request_duration_seconds";
    pub const GRPC_ERRORS_TOTAL: &str = "zaino.grpc.errors_total";
    pub const JSONRPC_ERRORS_TOTAL: &str = "zaino.jsonrpc.errors_total";

    /// Cardinality bounded: gRPC by `stringify!` in the handler macro, JSON-RPC by the method table
    pub const SERVE_METHOD: &str = "method";

    /// gRPC status name (`NotFound`) or JSON-RPC numeric code; never caller-supplied
    pub const SERVE_CODE: &str = "code";

    #[rustfmt::skip]
    pub const COUNTERS: &[(&str, &str)] = &[
        (GRPC_ERRORS_TOTAL, "Inbound gRPC errors by method and canonical status name"),
        (JSONRPC_ERRORS_TOTAL, "Inbound JSON-RPC errors by method and zcashd-compatible error code"),
    ];

    #[rustfmt::skip]
    pub const HISTOGRAMS: &[(&str, &str)] = &[
        (GRPC_REQUEST_DURATION_SECONDS, "Seconds serving one inbound gRPC request, by method; streaming methods time setup only"),
        (JSONRPC_REQUEST_DURATION_SECONDS, "Seconds serving one inbound JSON-RPC request, by method"),
    ];
}
