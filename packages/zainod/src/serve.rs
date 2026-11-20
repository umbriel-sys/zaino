//! The gRPC and JSON-RPC servers and the services they serve: the
//! lightwalletd `CompactTxStreamer` and the zcash JSON-RPC methods.

use tokio::sync::watch;
use zaino_proto::proto::indexed_tip::indexed_tip_service_server::IndexedTipServiceServer;
use zaino_proto::proto::service::compact_tx_streamer_server::CompactTxStreamerServer;
use zaino_state::{IndexedTipIndexer, IndexerSubscriber, LightWalletIndexer, ZcashIndexer};

pub mod config;
pub mod error;
pub mod grpc;
pub mod jsonrpc;

use grpc::indexed_tip::IndexedTipService;

#[derive(Clone)]
/// Zaino gRPC service.
pub struct GrpcClient<Indexer: ZcashIndexer + LightWalletIndexer> {
    /// Chain fetch service subscriber.
    pub service_subscriber: IndexerSubscriber<Indexer>,
}

#[derive(Clone)]
/// Zaino JSONRPC service.
pub struct JsonRpcClient<Indexer: ZcashIndexer + LightWalletIndexer> {
    /// Chain fetch service subscriber.
    pub service_subscriber: IndexerSubscriber<Indexer>,
}

/// The `CompactTxStreamer` and indexed-tip services over
/// `service_subscriber`, as routes for [`grpc::TonicServer`].
pub fn grpc_routes<Indexer: ZcashIndexer + LightWalletIndexer + IndexedTipIndexer + Clone>(
    service_subscriber: IndexerSubscriber<Indexer>,
    shutdown: watch::Receiver<()>,
) -> tonic::service::Routes {
    let indexed_tip_service = IndexedTipService::new(service_subscriber.inner_clone(), shutdown);
    tonic::service::Routes::new(CompactTxStreamerServer::new(GrpcClient {
        service_subscriber,
    }))
    .add_service(IndexedTipServiceServer::new(indexed_tip_service))
}

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
