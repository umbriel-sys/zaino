//! What each lightwalletd request reads, and how its answer and its
//! refusals are worded.

use std::collections::HashSet;
use std::sync::Arc;

use futures::{Stream, StreamExt as _};
use tracing::warn;
use zaino_chain::{
    AddressRead as _, BlockId as At, BlockRead as _, ChainViewError, ChainViewSnapshot as _,
    CompactBlockRead as _, CompactFrameRead as _, TreestateRead as _,
};
use zaino_primitives::types::{
    BlockHash, Height, Network, PoolFilter, ShieldedPool, TransactionId, TransparentAddress,
    Treestate, Utxo, Zatoshis,
};
use zaino_proto::compact::{
    compact_block_to_wire, compact_tx_to_wire, pool_filter_from_wire, whole_compact_block_to_wire,
};
use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::light_client::BlockHeader;
use zaino_proto::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, GetAddressUtxosArg, GetAddressUtxosReply,
    GetAddressUtxosReplyList, GetMempoolTxRequest, GetSubtreeRootsArg, LightdInfo, PingResponse,
    RawTransaction, SendResponse, ShieldedProtocol, StandardFee, SubtreeRoot,
    TransparentAddressBlockFilter, TreeState, TxFilter,
};
use zaino_proto::proto::utils::{GetBlockRangeError, PoolTypeFilter, ValidatedBlockRangeRequest};
use zaino_service::{ChainSnapshot, Placement, ServiceError, ServiceSource, TransactionStatus};
use zaino_source::{GetTreestateByHashError, GetTreestateError, UtxoPage};

use super::status::{self, Kind, Refusal};
use super::stream::{spawn_timed_stream, ChannelStream, StreamBounds};
use super::Lightwallet;

/// The most addresses one `GetAddressUtxos` request may carry.
const UTXO_MAX_ADDRESSES: usize = 1000;

/// The most distinct addresses one `GetTaddressBalanceStream` may send, as
/// lightwalletd's `maxTaddrsPerRequest`.
const BALANCE_STREAM_MAX_ADDRESSES: usize = 10_000;

/// zebrad's code for bytes that are not a transaction.
const DESERIALIZATION_CODE: i32 = -22;

/// A block named by a request.
#[derive(Debug, Clone, Copy)]
enum Wanted {
    Hash(BlockHash),
    Height(u32),
}

/// The block a `BlockId` names: a 32-byte hash, else the height.
fn wanted(block_id: BlockId) -> Option<Wanted> {
    match <[u8; 32]>::try_from(block_id.hash) {
        Ok(hash) => Some(Wanted::Hash(BlockHash::from(hash))),
        Err(_) => u32::try_from(block_id.height).ok().map(Wanted::Height),
    }
}

/// The pools a compact-block read serves: the requested ones, or for a
/// nullifier read only their shielded nullifiers (D6-18).
fn served_pools(requested: &PoolTypeFilter, nullifiers_only: bool) -> PoolFilter {
    let pools = pool_filter_from_wire(requested);
    match nullifiers_only {
        false => pools,
        true => ShieldedPool::ALL
            .into_iter()
            .filter(|pool| pools.includes(*pool))
            .fold(PoolFilter::none(), PoolFilter::with_pool)
            .nullifiers_only(),
    }
}

/// A range the tip does not reach, as lightwalletd words it.
fn above_the_tip(height: u32, tip: u32) -> tonic::Status {
    tonic::Status::out_of_range(format!(
        "Error: Height out of range [{height}]. Height requested is greater than the best chain \
         tip [{tip}]."
    ))
}

/// A malformed `BlockRange`, as lightwalletd refuses it.
fn range_request_status(error: GetBlockRangeError) -> tonic::Status {
    match error {
        GetBlockRangeError::StartHeightOutOfRange => tonic::Status::out_of_range(
            "Error: Start height out of range. Failed to convert to u32.",
        ),
        GetBlockRangeError::NoStartHeightProvided => {
            tonic::Status::out_of_range("Error: No start height given")
        }
        GetBlockRangeError::EndHeightOutOfRange => {
            tonic::Status::out_of_range("Error: End height out of range. Failed to convert to u32.")
        }
        GetBlockRangeError::NoEndHeightProvided => {
            tonic::Status::out_of_range("Error: No end height given.")
        }
        GetBlockRangeError::PoolTypeArgumentError(_) => invalid_pool_type(),
    }
}

fn invalid_pool_type() -> tonic::Status {
    tonic::Status::invalid_argument("Error: invalid pool type")
}

/// The addresses of an address-index request, checked as zebrad checks
/// them.
fn checked_addresses(addresses: &[String]) -> Result<Vec<TransparentAddress>, Refusal> {
    crate::serve::addresses::checked_addresses(addresses).map_err(Refusal::rejected_query)
}

/// A `GetTaddressTransactions` filter as lightwalletd reads it: a range and
/// its start are required; an absent end is 0, which the range rule reads
/// as the tip. The bounds are never swapped.
fn taddress_filter(
    request: TransparentAddressBlockFilter,
) -> Result<(String, u32, u32), tonic::Status> {
    let height = |block: BlockId, out_of_range: &'static str| {
        u32::try_from(block.height).map_err(|_| tonic::Status::invalid_argument(out_of_range))
    };
    let range = request
        .range
        .ok_or_else(|| tonic::Status::invalid_argument("Error: No block range given."))?;
    let start = range
        .start
        .ok_or_else(|| tonic::Status::invalid_argument("Error: No start height given."))?;
    let start = height(
        start,
        "Error: Start height out of range. Failed to convert to u32.",
    )?;
    let end = match range.end {
        Some(end) => height(
            end,
            "Error: End height out of range. Failed to convert to u32.",
        )?,
        None => 0,
    };
    Ok((request.address, start, end))
}

/// A `GetSubtreeRoots` request's pool, start index and limit (`max_entries`
/// 0: none).
fn subtree_roots_request(
    request: &GetSubtreeRootsArg,
) -> Result<(ShieldedPool, u16, Option<u16>), tonic::Status> {
    let pool = match ShieldedProtocol::try_from(request.shielded_protocol) {
        Ok(ShieldedProtocol::Sapling) => ShieldedPool::Sapling,
        Ok(ShieldedProtocol::Orchard) => ShieldedPool::Orchard,
        Ok(ShieldedProtocol::Ironwood) => ShieldedPool::Ironwood,
        Err(_) => {
            return Err(tonic::Status::invalid_argument(
                "Error: Invalid shielded protocol value.",
            ))
        }
    };
    let start_index = u16::try_from(request.start_index).map_err(|_| {
        tonic::Status::invalid_argument("Error: start_index value exceeds u16 range.")
    })?;
    let limit = match request.max_entries {
        0 => None,
        max_entries => Some(u16::try_from(max_entries).map_err(|_| {
            tonic::Status::invalid_argument("Error: max_entries value exceeds u16 range.")
        })?),
    };
    Ok((pool, start_index, limit))
}

/// The request's page: its `start_height` and `max_entries` (0: no bound but
/// the cap), as lightwalletd passes them to the validator (#46).
fn utxo_page(start_height: u64, max_entries: u32) -> Result<UtxoPage, tonic::Status> {
    let start = u32::try_from(start_height)
        .ok()
        .and_then(|height| Height::try_from(height).ok())
        .ok_or_else(|| tonic::Status::invalid_argument("Error: Start height out of range."))?;
    let max_entries = usize::try_from(max_entries)
        .map_err(|_| tonic::Status::invalid_argument("Error: Max entries out of range."))?;
    UtxoPage::new(start, max_entries)
        .map_err(|error| tonic::Status::invalid_argument(format!("Error: {error}.")))
}

/// A UTXO as the light-wallet protocol's reply.
fn utxo_reply(utxo: Utxo) -> Result<GetAddressUtxosReply, tonic::Status> {
    let Utxo {
        address,
        txid,
        output_index,
        script,
        satoshis,
        height,
    } = utxo;
    Ok(GetAddressUtxosReply {
        address: String::from(address),
        txid: <[u8; 32]>::from(txid).to_vec(),
        index: i32::try_from(output_index).map_err(|_| {
            tonic::Status::unknown("Error: Index out of range. Failed to convert to i32.")
        })?,
        script: Vec::<u8>::from(script),
        value_zat: i64::try_from(u64::from(satoshis)).map_err(|_| {
            tonic::Status::unknown("Error: Satoshis out of range. Failed to convert to i64.")
        })?,
        height: u64::from(u32::from(height)),
    })
}

/// A balance as the protocol's signed zatoshis.
fn balance_reply(zatoshis: u64) -> Result<Balance, tonic::Status> {
    Ok(Balance {
        value_zat: i64::try_from(zatoshis).map_err(|_| {
            tonic::Status::unknown("Error: Error converting balance from u64 to i64.")
        })?,
    })
}

/// A txid as RPCs display it.
fn display_txid(txid: TransactionId) -> String {
    let mut bytes = <[u8; 32]>::from(txid);
    bytes.reverse();
    hex::encode(bytes)
}

/// The height lightwalletd reports for a transaction: its best-chain
/// block's, `u64::MAX` if mined only off the best chain, 0 in a mempool;
/// `None` when nothing places it.
pub(super) fn light_wallet_height(
    status: &TransactionStatus,
) -> Result<Option<u64>, tonic::Status> {
    match status.best {
        Some(Placement::Block(position)) => {
            return Ok(Some(u64::from(u32::from(position.block.height))))
        }
        Some(Placement::Mempool(Some(_))) => return Ok(Some(0)),
        Some(Placement::Mempool(None)) => {
            return Err(tonic::Status::internal(
                "a coherent mempool placement without a height",
            ))
        }
        None => {}
    }
    let off_best = |mined: bool| {
        status
            .elsewhere
            .iter()
            .any(|placement| matches!(placement, Placement::Block(_)) == mined)
    };
    Ok(match off_best(true) {
        true => Some(u64::MAX),
        false => off_best(false).then_some(0),
    })
}

/// The gRPC `TreeState` of a treestate: hex per-pool final states, empty
/// where the pool is not active, as lightwalletd.
fn tree_state_reply(network: Network, treestate: Treestate) -> TreeState {
    fn final_state(pool: Option<zaino_primitives::types::PoolTreestate>) -> String {
        pool.map(|pool| hex::encode(pool.final_state))
            .unwrap_or_default()
    }
    TreeState {
        // BIP 70's names: regtest is a test network
        network: match network {
            Network::Mainnet => "main",
            Network::PubTestnet | Network::Regtest => "test",
        }
        .to_owned(),
        height: u64::from(u32::from(treestate.height)),
        hash: treestate.block_hash.to_string(),
        time: treestate.time,
        sapling_tree: final_state(treestate.sapling),
        orchard_tree: final_state(treestate.orchard),
        ironwood_tree: final_state(treestate.ironwood),
    }
}

impl<S: ServiceSource> Lightwallet<S> {
    /// A stream's bounds: `multiple` × the configured timeout.
    fn bounds(&self, multiple: u32) -> StreamBounds {
        StreamBounds {
            limit: self.backend.info.stream_timeout.saturating_mul(multiple),
            buffer: self.backend.info.stream_channel_size,
            shutdown: self.shutdown.clone(),
        }
    }

    /// The tip of the best chain.
    pub(super) fn latest_block(&self) -> BlockId {
        let tip = self.backend.service.chain_snapshot().tip();
        BlockId {
            height: u64::from(u32::from(tip.height)),
            hash: <[u8; 32]>::from(tip.hash).to_vec(),
        }
    }

    /// Where Sapling activates, as the validator schedules it.
    async fn sapling_activation(&self) -> Option<u32> {
        let info = self
            .backend
            .service
            .snapshot()
            .blockchain_info()
            .await
            .ok()?;
        zaino_service::schedule_at(&info.upgrades, info.blocks)
            .sapling_activation
            .map(u32::from)
    }

    /// One whole block: every transaction, with the data of `pools`; `None`
    /// above the tip.
    async fn whole_block(
        &self,
        chain: &ChainSnapshot<S>,
        height: u32,
        pools: PoolFilter,
    ) -> Result<Option<CompactBlock>, Refusal> {
        let Some(at) = Height::try_from(height)
            .ok()
            .filter(|at| *at <= chain.tip().height)
        else {
            return Ok(None);
        };
        let block = chain
            .compact_block(at, pools)
            .await
            .map_err(Refusal::of_chain)?
            .ok_or_else(|| Refusal::missing_block(height))?;
        Ok(Some(whole_compact_block_to_wire(&block, pools)))
    }

    /// `GetBlock` and `GetBlockNullifiers`.
    pub(super) async fn compact_block(
        &self,
        request: BlockId,
        nullifiers_only: bool,
    ) -> Result<CompactBlock, tonic::Status> {
        let wanted = wanted(request).ok_or_else(|| {
            tonic::Status::invalid_argument(
                "Error: Invalid hash and/or height out of range. Failed to convert to u32.",
            )
        })?;
        let chain = self.backend.service.chain_snapshot();
        let height =
            match wanted {
                Wanted::Height(height) => height,
                Wanted::Hash(hash) => match chain.block_height(hash).await {
                    Ok(Some(height)) => u32::from(height),
                    Ok(None) => return Err(tonic::Status::invalid_argument(
                        "Error: Invalid hash and/or height out of range. Hash not found in chain",
                    )),
                    Err(error) => return Err(status::of_chain(error)),
                },
            };

        // `BlockID` has no `poolTypes`: one block is the whole block, every
        // pool and every transaction, as lightwalletd serves it (#18, #19);
        // a nullifier read keeps each transaction with only its nullifiers.
        let pools = served_pools(&PoolTypeFilter::includes_all(), nullifiers_only);
        let failure = match self.whole_block(&chain, height, pools).await {
            Ok(Some(block)) => return Ok(block),
            Ok(None) => tonic::Status::not_found("Error: Failed to retrieve block from state."),
            Err(refusal) => refusal.status(),
        };

        let tip = u32::from(chain.tip().height);
        Err(match wanted {
            Wanted::Height(height) if height > tip => above_the_tip(height, tip),
            Wanted::Height(height) if nullifiers_only => match self.sapling_activation().await {
                Some(sapling) if height < sapling => tonic::Status::out_of_range(format!(
                    "Error: Height out of range [{height}]. Height requested is below sapling \
                     activation height [{sapling}]."
                )),
                _ => failure,
            },
            _ => failure,
        })
    }

    /// `GetBlockRange` and `GetBlockRangeNullifiers`: the range in either
    /// direction, cut at the tip, then the first height not served.
    pub(super) fn block_range(
        &self,
        request: &BlockRange,
        rpc_name: &'static str,
        nullifiers_only: bool,
    ) -> Result<ChannelStream<CompactBlock>, tonic::Status> {
        self.ranged(
            request,
            rpc_name,
            nullifiers_only,
            |chain, from, to, pools| {
                chain.stream_compact(from, to, pools).map(move |chunk| {
                    chunk.map(|blocks| {
                        blocks
                            .iter()
                            .map(|block| compact_block_to_wire(block, pools))
                            .collect()
                    })
                })
            },
        )
    }

    /// The same ranges as encoded messages: each block's frame is written
    /// once and shared by every client through the view's frame cache.
    pub(super) fn block_range_frames(
        &self,
        request: &BlockRange,
        rpc_name: &'static str,
        nullifiers_only: bool,
    ) -> Result<ChannelStream<bytes::Bytes>, tonic::Status> {
        self.ranged(
            request,
            rpc_name,
            nullifiers_only,
            |chain, from, to, pools| chain.stream_compact_frames(from, to, pools),
        )
    }

    /// `GetBlockHeaders`: the headers of a range, under the block ranges'
    /// rules. A header Zaino holds is its own; the pools are not read.
    pub(super) fn block_headers(
        &self,
        request: &BlockRange,
    ) -> Result<ChannelStream<BlockHeader>, tonic::Status> {
        self.ranged(request, "GetBlockHeaders", false, |chain, from, to, _| {
            chain.stream_headers(from, to).map(|chunk| {
                chunk.map(|headers| {
                    headers
                        .iter()
                        .map(|header| BlockHeader {
                            height: u64::from(u32::from(header.height)),
                            hash: <[u8; 32]>::from(header.hash).to_vec(),
                            header: zaino_encoding::encode_header(header),
                        })
                        .collect()
                })
            })
        })
    }

    /// A block range served from `chunks`.
    fn ranged<T, Chunks>(
        &self,
        request: &BlockRange,
        rpc_name: &'static str,
        nullifiers_only: bool,
        chunks: impl FnOnce(&ChainSnapshot<S>, Height, Height, PoolFilter) -> Chunks + Send + 'static,
    ) -> Result<ChannelStream<T>, tonic::Status>
    where
        T: Send + 'static,
        Chunks: Stream<Item = Result<Vec<T>, ChainViewError>> + Send,
    {
        let validated = ValidatedBlockRangeRequest::new_from_block_range(request)
            .map_err(range_request_status)?;
        let pools = served_pools(validated.pool_type_filter(), nullifiers_only);
        let (start, end) = (validated.start(), validated.end());
        let chain = self.backend.service.chain_snapshot();
        let tip = u32::from(chain.tip().height);

        Ok(spawn_timed_stream(
            self.bounds(4),
            tonic::Status::deadline_exceeded(format!("Error: {rpc_name} gRPC request timed out.")),
            |sender| async move {
                let ascending = start <= end;
                if !ascending && start > tip {
                    if sender.send(Err(above_the_tip(start, tip))).await.is_err() {
                        warn!("{rpc_name} channel closed unexpectedly");
                    }
                    return;
                }
                // Ascending past the tip: serve up to the tip, then say what was cut.
                let past_tip = ascending && end > tip;
                let last = if past_tip { tip } else { end };
                if let (true, Ok(from), Ok(to)) = (
                    !ascending || start <= tip,
                    Height::try_from(start),
                    Height::try_from(last),
                ) {
                    let mut chunks = std::pin::pin!(chunks(&chain, from, to, pools));
                    while let Some(chunk) = chunks.next().await {
                        let blocks = match chunk {
                            Ok(blocks) => blocks.into_iter().map(Ok).collect(),
                            Err(error) => vec![Err(tonic::Status::internal(
                                Refusal::of_chain(error).line(),
                            ))],
                        };
                        for block in blocks {
                            if sender.send(block).await.is_err() {
                                return;
                            }
                        }
                    }
                }
                // the first height not served, as lightwalletd names it
                if past_tip
                    && sender
                        .send(Err(above_the_tip(start.max(tip.saturating_add(1)), tip)))
                        .await
                        .is_err()
                {
                    // the client went away: nobody to tell
                }
            },
        ))
    }

    /// A transaction as the light-wallet `RawTransaction`, at its
    /// [`light_wallet_height`]; `Unavailable` while nothing places it.
    async fn light_wallet_transaction(
        &self,
        chain: &ChainSnapshot<S>,
        txid: TransactionId,
    ) -> Result<RawTransaction, tonic::Status> {
        let reads = self.backend.service.with_chain(chain.clone());
        let served = reads
            .transaction(txid)
            .await
            .map_err(status::of_service)?
            .ok_or_else(|| {
                tonic::Status::not_found(format!("transaction not found: {}", display_txid(txid)))
            })?;
        let placed = match reads.transaction_status(txid).await {
            Ok(placed) => placed,
            Err(error @ ServiceError::MinedAndInMempool(_)) => {
                return Err(Refusal::new(Kind::InvalidSnapshot, error.to_string()).status())
            }
            Err(error) => return Err(status::of_service(error)),
        };
        // bytes without a location (the validator's, read before the indexes
        // place it) are not a mempool transaction: refused, to be asked again
        let height = light_wallet_height(&placed)?.ok_or_else(|| {
            tonic::Status::unavailable(format!(
                "transaction {} is not placed yet",
                display_txid(txid)
            ))
        })?;
        Ok(RawTransaction {
            data: bytes::Bytes::from(served.bytes),
            height,
        })
    }

    /// `GetTransaction`.
    pub(super) async fn transaction(
        &self,
        request: TxFilter,
    ) -> Result<RawTransaction, tonic::Status> {
        let hash = <[u8; 32]>::try_from(request.hash)
            .map_err(|_| tonic::Status::invalid_argument("Error: Transaction hash incorrect"))?;
        let chain = self.backend.service.chain_snapshot();
        self.light_wallet_transaction(&chain, TransactionId::from(hash))
            .await
    }

    /// `SendTransaction`, as lightwalletd (#38): the quoted txid on success;
    /// a rejection is an OK reply carrying the validator's code and message;
    /// unreachable is `UNAVAILABLE`.
    pub(super) async fn send_transaction(
        &self,
        request: RawTransaction,
    ) -> Result<SendResponse, tonic::Status> {
        // refused before the validator round trip, with zebrad's code
        if let Err(error) = zaino_consensus::validate_raw_transaction_bytes(&request.data) {
            return Ok(SendResponse {
                error_code: DESERIALIZATION_CODE,
                error_message: error.to_string(),
            });
        }
        let sent = self
            .backend
            .service
            .snapshot()
            .send_raw_transaction(request.data.to_vec())
            .await;
        let refusal = match sent {
            Ok(txid) => {
                return Ok(SendResponse {
                    error_code: 0,
                    error_message: format!("\"{txid}\""),
                })
            }
            Err(error) => Refusal::of_service(error),
        };
        let rejection = refusal
            .rejection
            .as_ref()
            .and_then(|(code, message)| Some((i32::try_from(*code).ok()?, message.clone())));
        match rejection {
            Some((error_code, error_message)) => Ok(SendResponse {
                error_code,
                error_message,
            }),
            None => Err(refusal.status_unavailable_if_unreachable()),
        }
    }

    /// `GetTaddressTransactions` (and the deprecated `GetTaddressTxids`): the
    /// ids and the transactions from one snapshot (#49).
    pub(super) async fn taddress_transactions(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<ChannelStream<RawTransaction>, tonic::Status> {
        let (address, start, end) = taddress_filter(request)?;
        let chain = self.backend.service.chain_snapshot();
        let addresses = checked_addresses(&[address]).map_err(Refusal::status)?;
        let txids = self
            .backend
            .service
            .with_chain(chain.clone())
            .address_txids(&addresses, Some(start), Some(end))
            .await
            .map_err(status::of_service)?;
        let service = self.clone();
        Ok(spawn_timed_stream(
            self.bounds(4),
            tonic::Status::internal("Error: get_taddress_transactions gRPC request timed out"),
            |sender| async move {
                for txid in txids {
                    let transaction = service.light_wallet_transaction(&chain, txid).await;
                    if sender.send(transaction).await.is_err() {
                        break;
                    }
                }
            },
        ))
    }

    /// The balance of `addresses` at one snapshot.
    async fn balance_of(&self, addresses: &[String]) -> Result<u64, tonic::Status> {
        let chain = self.backend.service.chain_snapshot();
        let addresses = checked_addresses(addresses).map_err(Refusal::status)?;
        let balance = chain
            .address_balance(&addresses)
            .await
            .map_err(status::of_chain)?;
        Ok(u64::from(balance.balance))
    }

    /// `GetTaddressBalance`.
    pub(super) async fn taddress_balance(
        &self,
        request: AddressList,
    ) -> Result<Balance, tonic::Status> {
        balance_reply(self.balance_of(&request.addresses).await?)
    }

    /// `GetTaddressBalanceStream`: each address once, at most the limit, then
    /// one query over one snapshot (Q-45, as lightwalletd).
    pub(super) async fn taddress_balance_stream(
        &self,
        mut request: impl Stream<Item = Result<Address, tonic::Status>> + Unpin,
    ) -> Result<Balance, tonic::Status> {
        let total = tokio::time::timeout(self.bounds(4).limit, async {
            let mut seen = HashSet::new();
            let mut addresses = Vec::new();
            while let Some(address) = request.next().await {
                let address = address
                    .map_err(|error| {
                        tonic::Status::unknown(format!("Failed to read from stream: {error}"))
                    })?
                    .address;
                if !seen.insert(address.clone()) {
                    continue;
                }
                if seen.len() > BALANCE_STREAM_MAX_ADDRESSES {
                    return Err(tonic::Status::invalid_argument(format!(
                        "Error: too many addresses in request: the maximum is \
                         {BALANCE_STREAM_MAX_ADDRESSES}."
                    )));
                }
                addresses.push(address);
            }
            self.balance_of(&addresses).await
        })
        .await
        .unwrap_or_else(|_| {
            Err(tonic::Status::deadline_exceeded(
                "Error: get_taddress_balance_stream request timed out.",
            ))
        })?;
        balance_reply(total)
    }

    /// `GetMempoolTx`, as lightwalletd: no pools is the shielded ones, an
    /// invalid pool is refused, and a transaction with nothing in them is
    /// dropped (#44).
    pub(super) fn mempool_tx(
        &self,
        request: GetMempoolTxRequest,
    ) -> Result<ChannelStream<CompactTx>, tonic::Status> {
        let exclude = request.exclude_txid_suffixes;
        let pools = PoolTypeFilter::new_from_slice(&request.pool_types)
            .map(|filter| pool_filter_from_wire(&filter))
            .map_err(|_| invalid_pool_type())?;
        let service = Arc::clone(&self.backend.service);
        Ok(spawn_timed_stream(
            self.bounds(4),
            tonic::Status::internal("Error: get_mempool_tx gRPC request timed out"),
            |sender| async move {
                let entries = match service.mempool_entries(&exclude) {
                    Ok(entries) => entries,
                    Err(error) => {
                        if sender.send(Err(status::of_service(error))).await.is_err() {
                            // the client went away: nobody to tell
                        }
                        return;
                    }
                };
                for entry in entries {
                    // the entry's compact form, decoded once at ingest
                    let compact = entry.compact.clone().filtered(pools);
                    if !compact.has_pool_data() {
                        continue;
                    }
                    // a mempool transaction is in no block, so index 0
                    if sender
                        .send(Ok(compact_tx_to_wire(0, &compact)))
                        .await
                        .is_err()
                    {
                        break;
                    }
                }
            },
        ))
    }

    /// `GetMempoolStream`: the mempool coherent with the tip this request was
    /// admitted against, until that tip moves.
    pub(super) fn mempool_stream(&self) -> ChannelStream<RawTransaction> {
        let service = Arc::clone(&self.backend.service);
        let chain = service.chain_snapshot();
        spawn_timed_stream(
            self.bounds(6),
            tonic::Status::internal("Error: get_mempool_stream gRPC request timed out"),
            |sender| async move {
                let Some(transactions) = service.mempool_stream(Some(&chain)) else {
                    warn!("mempool stream requested against a stale snapshot");
                    if sender
                        .send(Err(tonic::Status::failed_precondition(
                            "mempool is not coherent with the requested snapshot; retry with a \
                             fresh snapshot",
                        )))
                        .await
                        .is_err()
                    {
                        // the client went away: nobody to tell
                    }
                    return;
                };
                let mut transactions = std::pin::pin!(transactions);
                while let Some(transaction) = transactions.next().await {
                    // unmined: 0 on the wire, never the tip
                    let item = transaction
                        .map(|data| RawTransaction { data, height: 0 })
                        .map_err(|error| {
                            tonic::Status::unavailable(format!(
                                "mempool stream ended early: {error}"
                            ))
                        });
                    let ended = item.is_err();
                    if sender.send(item).await.is_err() || ended {
                        break;
                    }
                }
            },
        )
    }

    /// The treestate at `at`, refused as the validator's port refuses a
    /// block it does not hold.
    async fn treestate_at(
        &self,
        chain: &ChainSnapshot<S>,
        at: At,
    ) -> Result<TreeState, tonic::Status> {
        let treestate = chain
            .treestate(at)
            .await
            .map_err(status::of_chain)?
            .ok_or_else(|| {
                match at {
                    At::Hash(hash) => {
                        Refusal::validator_refused(GetTreestateByHashError::BlockNotFound(hash))
                    }
                    At::Height(height) => {
                        Refusal::validator_refused(GetTreestateError::HeightNotFound(height))
                    }
                }
                .status()
            })?;
        Ok(tree_state_reply(self.backend.info.network, treestate))
    }

    /// `GetTreeState`: by hash or height.
    pub(super) async fn tree_state(&self, request: BlockId) -> Result<TreeState, tonic::Status> {
        let at = match wanted(request)
            .ok_or_else(|| tonic::Status::invalid_argument("Invalid hash or height"))?
        {
            Wanted::Hash(hash) => At::Hash(hash),
            Wanted::Height(height) => At::Height(
                Height::try_from(height)
                    .map_err(|error| Refusal::validator_input(error).status())?,
            ),
        };
        let chain = self.backend.service.chain_snapshot();
        self.treestate_at(&chain, at).await
    }

    /// `GetLatestTreeState`: by hash, so the treestate is the tip's even if
    /// the chain moves.
    pub(super) async fn latest_tree_state(&self) -> Result<TreeState, tonic::Status> {
        let chain = self.backend.service.chain_snapshot();
        self.treestate_at(&chain, At::Hash(chain.tip().hash)).await
    }

    /// `GetSubtreeRoots`: the roots and their completing blocks, all from one
    /// snapshot (#49).
    pub(super) async fn subtree_roots(
        &self,
        request: GetSubtreeRootsArg,
    ) -> Result<ChannelStream<SubtreeRoot>, tonic::Status> {
        let (pool, start_index, limit) = subtree_roots_request(&request)?;
        let chain = self.backend.service.chain_snapshot();
        let subtrees = chain
            .subtree_roots(pool, start_index, limit)
            .await
            .map_err(status::of_chain)?;
        Ok(spawn_timed_stream(
            self.bounds(4),
            tonic::Status::deadline_exceeded("Error: get_subtree_roots gRPC request timed out"),
            |sender| async move {
                for subtree in subtrees {
                    let height = subtree.end_height;
                    let reply = match chain.block_hash(height).await {
                        Ok(Some(hash)) => {
                            let mut display = <[u8; 32]>::from(hash);
                            display.reverse();
                            Ok(SubtreeRoot {
                                root_hash: <[u8; 32]>::from(subtree.root).to_vec(),
                                completing_block_hash: display.to_vec(),
                                completing_block_height: u64::from(u32::from(height)),
                            })
                        }
                        Ok(None) => Err(tonic::Status::internal(format!(
                            "Error: no block at {height}, where a subtree completed."
                        ))),
                        Err(error) => Err(status::of_chain(error)),
                    };
                    let failed = reply.is_err();
                    if sender.send(reply).await.is_err() || failed {
                        return;
                    }
                }
            },
        ))
    }

    /// The page of unspent outputs a `GetAddressUtxos` request asks for.
    async fn requested_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<Vec<Utxo>, tonic::Status> {
        if request.addresses.len() > UTXO_MAX_ADDRESSES {
            return Err(tonic::Status::invalid_argument(format!(
                "Error: too many addresses in request: {} exceeds the maximum of \
                 {UTXO_MAX_ADDRESSES}.",
                request.addresses.len()
            )));
        }
        let page = utxo_page(request.start_height, request.max_entries)?;
        let chain = self.backend.service.chain_snapshot();
        let addresses = checked_addresses(&request.addresses).map_err(Refusal::status)?;
        chain
            .address_utxos(&addresses, page)
            .await
            .map_err(status::of_chain)
    }

    /// `GetAddressUtxos`.
    pub(super) async fn address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, tonic::Status> {
        let address_utxos = self
            .requested_utxos(request)
            .await?
            .into_iter()
            .map(utxo_reply)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GetAddressUtxosReplyList { address_utxos })
    }

    /// `GetAddressUtxosStream`.
    pub(super) async fn address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<ChannelStream<GetAddressUtxosReply>, tonic::Status> {
        let utxos = self.requested_utxos(request).await?;
        Ok(spawn_timed_stream(
            self.bounds(4),
            tonic::Status::deadline_exceeded(
                "Error: get_address_utxos_stream gRPC request timed out",
            ),
            |sender| async move {
                for reply in utxos.into_iter().map(utxo_reply) {
                    let failed = reply.is_err();
                    if sender.send(reply).await.is_err() || failed {
                        return;
                    }
                }
            },
        ))
    }

    /// `GetLightdInfo`.
    pub(super) async fn lightd_info(&self) -> Result<LightdInfo, tonic::Status> {
        let info = &self.backend.info;
        let chain = self
            .backend
            .service
            .snapshot()
            .blockchain_info()
            .await
            .map_err(status::of_service)?;
        // proto3's zero and empty string are "not scheduled" (K-02)
        let schedule = zaino_service::schedule_at(&chain.upgrades, chain.blocks);
        let height = |height: Height| u64::from(u32::from(height));
        Ok(LightdInfo {
            version: info.build.version.to_owned(),
            vendor: "ZingoLabs ZainoD".to_owned(),
            taddr_support: true,
            chain_name: chain.chain.clone(),
            sapling_activation_height: schedule.sapling_activation.map_or(0, height),
            consensus_branch_id: chain.consensus.chain_tip.to_string(),
            block_height: height(chain.blocks),
            git_commit: info.build.commit.to_owned(),
            branch: info.build.branch.to_owned(),
            build_date: info.build.build_date.to_owned(),
            build_user: info.build.build_user.to_owned(),
            estimated_height: height(chain.estimated_height),
            zcashd_build: info.validator_build.clone(),
            zcashd_subversion: info.validator_subversion.clone(),
            donation_address: info.donation_address.clone().unwrap_or_default(),
            upgrade_name: schedule
                .next_upgrade
                .map(|upgrade| upgrade.name.clone())
                .unwrap_or_default(),
            upgrade_height: schedule
                .next_upgrade
                .map_or(0, |upgrade| height(upgrade.activation_height)),
            lightwallet_protocol_version: "v0.6.0".to_owned(),
        })
    }

    /// `GetStandardFee`, as lightwalletd (#59): unreachable is `UNAVAILABLE`,
    /// and a zero fee, which no sane validator reports, is refused.
    pub(super) async fn standard_fee(&self) -> Result<StandardFee, tonic::Status> {
        let fee = self
            .backend
            .service
            .snapshot()
            .standard_fee()
            .await
            .map_err(|error| Refusal::of_service(error).status_unavailable_if_unreachable())?;
        if fee.fee_per_action == Zatoshis::ZERO {
            return Err(tonic::Status::internal(
                "Error: the validator reported a zero standard fee.",
            ));
        }
        Ok(StandardFee {
            fee_zat: fee.fee_per_action.as_u64(),
            estimator_version: fee.version,
        })
    }

    /// `Ping`: testing-only in lightwalletd, behind `--ping-very-insecure`;
    /// never enabled here, and answered as lightwalletd does when disabled.
    pub(super) fn ping(&self) -> Result<PingResponse, tonic::Status> {
        Err(tonic::Status::failed_precondition(
            "Error: Ping is not enabled.",
        ))
    }
}
