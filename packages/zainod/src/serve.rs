//! The gRPC and JSON-RPC servers and the services they serve: the
//! lightwalletd `CompactTxStreamer` and the zcash JSON-RPC methods.

pub mod addresses;
pub mod backend;
pub mod config;
pub mod error;
pub mod grpc;
pub mod jsonrpc;
#[cfg(test)]
mod testing;

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
