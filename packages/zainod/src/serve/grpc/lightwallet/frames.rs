//! Block ranges served as encoded frames: the range requests are answered
//! with each block's already-encoded message, copied rather than re-encoded
//! per client; every other request goes to the generated server.

use std::convert::Infallible;
use std::task::{Context, Poll};

use bytes::{Buf as _, BufMut as _, Bytes};
use futures::future::BoxFuture;
use prost::Message as _;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::server::NamedService;
use zaino_proto::proto::service::{
    compact_tx_streamer_server::CompactTxStreamerServer, BlockRange,
};
use zaino_service::ServiceSource;

use super::stream::ChannelStream;
use super::{served, Lightwallet};

const GET_BLOCK_RANGE: &str = "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRange";
const GET_BLOCK_RANGE_NULLIFIERS: &str =
    "/cash.z.wallet.sdk.rpc.CompactTxStreamer/GetBlockRangeNullifiers";

/// The lightwalletd service with its block ranges served as frames.
pub(super) struct FramedRanges<S: ServiceSource> {
    lightwallet: Lightwallet<S>,
    generated: CompactTxStreamerServer<Lightwallet<S>>,
}

impl<S: ServiceSource> FramedRanges<S> {
    pub(super) fn new(lightwallet: Lightwallet<S>) -> Self {
        Self {
            generated: CompactTxStreamerServer::new(lightwallet.clone()),
            lightwallet,
        }
    }
}

impl<S: ServiceSource> Clone for FramedRanges<S> {
    fn clone(&self) -> Self {
        Self {
            lightwallet: self.lightwallet.clone(),
            generated: self.generated.clone(),
        }
    }
}

impl<S: ServiceSource> NamedService for FramedRanges<S> {
    const NAME: &'static str = <CompactTxStreamerServer<Lightwallet<S>> as NamedService>::NAME;
}

impl<S: ServiceSource> tower::Service<http::Request<tonic::body::Body>> for FramedRanges<S> {
    type Response = http::Response<tonic::body::Body>;
    type Error = Infallible;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        tower::Service::<http::Request<tonic::body::Body>>::poll_ready(&mut self.generated, cx)
    }

    fn call(&mut self, request: http::Request<tonic::body::Body>) -> Self::Future {
        let range = match request.uri().path() {
            GET_BLOCK_RANGE => Some(("get_block_range", "GetBlockRange", false)),
            GET_BLOCK_RANGE_NULLIFIERS => Some((
                "get_block_range_nullifiers",
                "GetBlockRangeNullifiers",
                true,
            )),
            _ => None,
        };
        let Some((method, rpc_name, nullifiers_only)) = range else {
            return self.generated.call(request);
        };
        let ranges = Ranges {
            lightwallet: self.lightwallet.clone(),
            method,
            rpc_name,
            nullifiers_only,
        };
        Box::pin(async move {
            Ok(tonic::server::Grpc::new(FrameCodec)
                .server_streaming(ranges, request)
                .await)
        })
    }
}

/// One range request, answered in frames.
struct Ranges<S: ServiceSource> {
    lightwallet: Lightwallet<S>,
    method: &'static str,
    rpc_name: &'static str,
    nullifiers_only: bool,
}

impl<S: ServiceSource> tonic::server::ServerStreamingService<BlockRange> for Ranges<S> {
    type Response = Bytes;
    type ResponseStream = ChannelStream<Bytes>;
    type Future = BoxFuture<'static, Result<tonic::Response<Self::ResponseStream>, tonic::Status>>;

    fn call(&mut self, request: tonic::Request<BlockRange>) -> Self::Future {
        let lightwallet = self.lightwallet.clone();
        let (method, rpc_name, nullifiers_only) =
            (self.method, self.rpc_name, self.nullifiers_only);
        Box::pin(served(method, async move {
            lightwallet.block_range_frames(&request.into_inner(), rpc_name, nullifiers_only)
        }))
    }
}

/// Reads a `BlockRange`; writes each frame's bytes as one message.
struct FrameCodec;

impl Codec for FrameCodec {
    type Encode = Bytes;
    type Decode = BlockRange;
    type Encoder = FrameCodec;
    type Decoder = FrameCodec;

    fn encoder(&mut self) -> Self::Encoder {
        FrameCodec
    }

    fn decoder(&mut self) -> Self::Decoder {
        FrameCodec
    }
}

impl Encoder for FrameCodec {
    type Item = Bytes;
    type Error = tonic::Status;

    fn encode(&mut self, frame: Bytes, out: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        out.put(frame);
        Ok(())
    }
}

impl Decoder for FrameCodec {
    type Item = BlockRange;
    type Error = tonic::Status;

    fn decode(&mut self, bytes: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        let length = bytes.remaining();
        BlockRange::decode(bytes.copy_to_bytes(length))
            .map(Some)
            .map_err(|error| tonic::Status::internal(error.to_string()))
    }
}
