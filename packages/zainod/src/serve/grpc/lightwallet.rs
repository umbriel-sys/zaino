//! The lightwalletd `CompactTxStreamer` service, answered from the indexer
//! service.
//!
//! Each request reads one snapshot of the service; each stream is fed by
//! one task that stops when its client stops reading.

mod frames;
mod reads;
mod status;
mod stream;
#[cfg(test)]
mod tests;

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use futures::{Stream, StreamExt as _};
use tokio::sync::watch;
use tracing::info;
use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::indexed_tip::{
    indexed_tip_service_server::{IndexedTipService, IndexedTipServiceServer},
    IndexedTip, SubscribeIndexedTipsRequest,
};
use zaino_proto::proto::light_client::{
    light_client_service_server::{LightClientService, LightClientServiceServer},
    BlockHeader, HistoryProof, HistoryProofRequest,
};
use zaino_proto::proto::service::{
    compact_tx_streamer_server::CompactTxStreamer, Address, AddressList, Balance, BlockId,
    BlockRange, ChainSpec, Duration, Empty, GetAddressUtxosArg, GetAddressUtxosReply,
    GetAddressUtxosReplyList, GetMempoolTxRequest, GetSubtreeRootsArg, LightdInfo, PingResponse,
    RawTransaction, SendResponse, StandardFee, SubtreeRoot, TransparentAddressBlockFilter,
    TreeState, TxFilter,
};
use zaino_service::ServiceSource;

use crate::serve::backend::Backend;

pub use stream::ChannelStream;

/// The lightwalletd service over a [`Backend`].
pub struct Lightwallet<S: ServiceSource> {
    backend: Backend<S>,
    shutdown: stream::Shutdown,
}

impl<S: ServiceSource> Lightwallet<S> {
    /// The service answering from `backend`.
    pub fn new(backend: Backend<S>) -> Self {
        Self {
            backend,
            shutdown: stream::Shutdown::default(),
        }
    }

    /// This service with its streams ending when `shutdown` changes or
    /// closes.
    fn ending_at(mut self, shutdown: watch::Receiver<()>) -> Self {
        self.shutdown = stream::Shutdown::on(shutdown);
        self
    }
}

impl<S: ServiceSource> Clone for Lightwallet<S> {
    fn clone(&self) -> Self {
        Self {
            backend: self.backend.clone(),
            shutdown: self.shutdown.clone(),
        }
    }
}

/// The lightwalletd service, Zaino's light-client service and the
/// indexed-tip service over `backend`, as routes for [`TonicServer`](super::TonicServer). Every stream ends when
/// `shutdown` changes or closes.
pub fn routes<S: ServiceSource>(
    backend: Backend<S>,
    shutdown: watch::Receiver<()>,
) -> tonic::service::Routes {
    let indexed_tips = IndexedTips {
        backend: backend.clone(),
        shutdown: shutdown.clone(),
    };
    let lightwallet = Lightwallet::new(backend).ending_at(shutdown);
    tonic::service::Routes::new(frames::FramedRanges::new(lightwallet.clone()))
        .add_service(LightClientServiceServer::new(lightwallet))
        .add_service(IndexedTipServiceServer::new(indexed_tips))
}

/// Zaino's own light-client requests, beside the lightwalletd ones.
#[tonic::async_trait]
impl<S: ServiceSource> LightClientService for Lightwallet<S> {
    type GetBlockHeadersStream = ChannelStream<BlockHeader>;

    async fn get_block_headers(
        &self,
        request: tonic::Request<BlockRange>,
    ) -> Result<tonic::Response<Self::GetBlockHeadersStream>, tonic::Status> {
        served("get_block_headers", async {
            self.block_headers(&request.into_inner())
        })
        .await
    }

    async fn get_history_proof(
        &self,
        request: tonic::Request<HistoryProofRequest>,
    ) -> Result<tonic::Response<HistoryProof>, tonic::Status> {
        served(
            "get_history_proof",
            self.history_proof(request.into_inner()),
        )
        .await
    }
}

/// One request's duration (whose `_count` is the per-method volume) and, on
/// error, a count keyed by the status's canonical name.
fn record_grpc_metrics<T>(
    method: &'static str,
    start: Instant,
    result: &Result<tonic::Response<T>, tonic::Status>,
) {
    use crate::serve::metric_names::*;
    metrics::histogram!(GRPC_REQUEST_DURATION_SECONDS, SERVE_METHOD => method)
        .record(start.elapsed().as_secs_f64());
    if let Err(status) = result {
        metrics::counter!(
            GRPC_ERRORS_TOTAL,
            SERVE_METHOD => method,
            SERVE_CODE => format!("{:?}", status.code()),
        )
        .increment(1);
    }
}

/// One request: logged, timed, and its error counted under `method`. A
/// streaming request is timed to the stream's start only.
async fn served<T>(
    method: &'static str,
    answer: impl Future<Output = Result<T, tonic::Status>>,
) -> Result<tonic::Response<T>, tonic::Status> {
    info!(method, "[TEST] received call");
    let start = Instant::now();
    let result = answer.await.map(tonic::Response::new);
    record_grpc_metrics(method, start, &result);
    result
}

#[tonic::async_trait]
impl<S: ServiceSource> CompactTxStreamer for Lightwallet<S> {
    type GetBlockRangeStream = ChannelStream<CompactBlock>;
    type GetBlockRangeNullifiersStream = ChannelStream<CompactBlock>;
    type GetTaddressTxidsStream = ChannelStream<RawTransaction>;
    type GetTaddressTransactionsStream = ChannelStream<RawTransaction>;
    type GetMempoolTxStream = ChannelStream<CompactTx>;
    type GetMempoolStreamStream = ChannelStream<RawTransaction>;
    type GetSubtreeRootsStream = ChannelStream<SubtreeRoot>;
    type GetAddressUtxosStreamStream = ChannelStream<GetAddressUtxosReply>;

    async fn get_latest_block(
        &self,
        _request: tonic::Request<ChainSpec>,
    ) -> Result<tonic::Response<BlockId>, tonic::Status> {
        served("get_latest_block", async { Ok(self.latest_block()) }).await
    }

    async fn get_block(
        &self,
        request: tonic::Request<BlockId>,
    ) -> Result<tonic::Response<CompactBlock>, tonic::Status> {
        served("get_block", self.compact_block(request.into_inner(), false)).await
    }

    async fn get_block_nullifiers(
        &self,
        request: tonic::Request<BlockId>,
    ) -> Result<tonic::Response<CompactBlock>, tonic::Status> {
        served(
            "get_block_nullifiers",
            self.compact_block(request.into_inner(), true),
        )
        .await
    }

    async fn get_block_range(
        &self,
        request: tonic::Request<BlockRange>,
    ) -> Result<tonic::Response<Self::GetBlockRangeStream>, tonic::Status> {
        served("get_block_range", async {
            self.block_range(&request.into_inner(), "GetBlockRange", false)
        })
        .await
    }

    async fn get_block_range_nullifiers(
        &self,
        request: tonic::Request<BlockRange>,
    ) -> Result<tonic::Response<Self::GetBlockRangeNullifiersStream>, tonic::Status> {
        served("get_block_range_nullifiers", async {
            self.block_range(&request.into_inner(), "GetBlockRangeNullifiers", true)
        })
        .await
    }

    async fn get_transaction(
        &self,
        request: tonic::Request<TxFilter>,
    ) -> Result<tonic::Response<RawTransaction>, tonic::Status> {
        served("get_transaction", self.transaction(request.into_inner())).await
    }

    async fn send_transaction(
        &self,
        request: tonic::Request<RawTransaction>,
    ) -> Result<tonic::Response<SendResponse>, tonic::Status> {
        served(
            "send_transaction",
            Lightwallet::send_transaction(self, request.into_inner()),
        )
        .await
    }

    async fn get_taddress_txids(
        &self,
        request: tonic::Request<TransparentAddressBlockFilter>,
    ) -> Result<tonic::Response<Self::GetTaddressTxidsStream>, tonic::Status> {
        served(
            "get_taddress_txids",
            self.taddress_transactions(request.into_inner()),
        )
        .await
    }

    async fn get_taddress_transactions(
        &self,
        request: tonic::Request<TransparentAddressBlockFilter>,
    ) -> Result<tonic::Response<Self::GetTaddressTransactionsStream>, tonic::Status> {
        served(
            "get_taddress_transactions",
            self.taddress_transactions(request.into_inner()),
        )
        .await
    }

    async fn get_taddress_balance(
        &self,
        request: tonic::Request<AddressList>,
    ) -> Result<tonic::Response<Balance>, tonic::Status> {
        served(
            "get_taddress_balance",
            self.taddress_balance(request.into_inner()),
        )
        .await
    }

    async fn get_taddress_balance_stream(
        &self,
        request: tonic::Request<tonic::Streaming<Address>>,
    ) -> Result<tonic::Response<Balance>, tonic::Status> {
        served(
            "get_taddress_balance_stream",
            self.taddress_balance_stream(request.into_inner()),
        )
        .await
    }

    async fn get_mempool_tx(
        &self,
        request: tonic::Request<GetMempoolTxRequest>,
    ) -> Result<tonic::Response<Self::GetMempoolTxStream>, tonic::Status> {
        served("get_mempool_tx", async {
            self.mempool_tx(request.into_inner())
        })
        .await
    }

    async fn get_mempool_stream(
        &self,
        _request: tonic::Request<Empty>,
    ) -> Result<tonic::Response<Self::GetMempoolStreamStream>, tonic::Status> {
        served("get_mempool_stream", async { Ok(self.mempool_stream()) }).await
    }

    async fn get_tree_state(
        &self,
        request: tonic::Request<BlockId>,
    ) -> Result<tonic::Response<TreeState>, tonic::Status> {
        served("get_tree_state", self.tree_state(request.into_inner())).await
    }

    async fn get_latest_tree_state(
        &self,
        _request: tonic::Request<Empty>,
    ) -> Result<tonic::Response<TreeState>, tonic::Status> {
        served("get_latest_tree_state", self.latest_tree_state()).await
    }

    async fn get_subtree_roots(
        &self,
        request: tonic::Request<GetSubtreeRootsArg>,
    ) -> Result<tonic::Response<Self::GetSubtreeRootsStream>, tonic::Status> {
        served(
            "get_subtree_roots",
            self.subtree_roots(request.into_inner()),
        )
        .await
    }

    async fn get_address_utxos(
        &self,
        request: tonic::Request<GetAddressUtxosArg>,
    ) -> Result<tonic::Response<GetAddressUtxosReplyList>, tonic::Status> {
        served(
            "get_address_utxos",
            self.address_utxos(request.into_inner()),
        )
        .await
    }

    async fn get_address_utxos_stream(
        &self,
        request: tonic::Request<GetAddressUtxosArg>,
    ) -> Result<tonic::Response<Self::GetAddressUtxosStreamStream>, tonic::Status> {
        served(
            "get_address_utxos_stream",
            self.address_utxos_stream(request.into_inner()),
        )
        .await
    }

    async fn get_lightd_info(
        &self,
        _request: tonic::Request<Empty>,
    ) -> Result<tonic::Response<LightdInfo>, tonic::Status> {
        served("get_lightd_info", self.lightd_info()).await
    }

    async fn get_standard_fee(
        &self,
        _request: tonic::Request<Empty>,
    ) -> Result<tonic::Response<StandardFee>, tonic::Status> {
        served("get_standard_fee", self.standard_fee()).await
    }

    async fn ping(
        &self,
        _request: tonic::Request<Duration>,
    ) -> Result<tonic::Response<PingResponse>, tonic::Status> {
        served("ping", async { Lightwallet::ping(self) }).await
    }
}

/// The indexed-tip service: the tip the service reads from, then each
/// change to it.
struct IndexedTips<S: ServiceSource> {
    backend: Backend<S>,
    shutdown: watch::Receiver<()>,
}

#[tonic::async_trait]
impl<S: ServiceSource> IndexedTipService for IndexedTips<S> {
    type SubscribeIndexedTipsStream =
        Pin<Box<dyn Stream<Item = Result<IndexedTip, tonic::Status>> + Send>>;

    async fn subscribe_indexed_tips(
        &self,
        _request: tonic::Request<SubscribeIndexedTipsRequest>,
    ) -> Result<tonic::Response<Self::SubscribeIndexedTipsStream>, tonic::Status> {
        let mut updates = self.backend.service.subscribe_tip();
        let initial = updates.borrow_and_update().best_tip;
        // latest state: a slow reader skips to the newest tip
        let changes = futures::stream::unfold(updates, |mut updates| async move {
            updates.changed().await.ok()?;
            let tip = updates.borrow_and_update().best_tip;
            Some((tip, updates))
        });
        let mut shutdown = self.shutdown.clone();
        let stream = futures::stream::once(async move { initial })
            .chain(changes)
            .map(|tip| {
                Ok(IndexedTip {
                    height: u32::from(tip.height),
                    hash: <[u8; 32]>::from(tip.hash).to_vec(),
                })
            })
            // a change, or the sender gone: either way the stream ends
            .take_until(async move {
                if shutdown.changed().await.is_err() {
                    // the owner is gone: end as well
                }
            });
        Ok(tonic::Response::new(Box::pin(stream)))
    }
}
