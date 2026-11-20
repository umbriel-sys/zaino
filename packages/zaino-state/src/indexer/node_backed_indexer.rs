//! Zcash chain fetch and tx submission service backed by the validator's JsonRPC service.

use crate::chain_index::chain_view::BestTip as _;
use futures::StreamExt;
use hex::FromHex;
use std::time;
use tokio::time::timeout;
use tracing::{info, instrument, warn};
use zaino_chain::ForkReconcile;
use zebra_state::HashOrHeight;

use zebra_chain::{
    block::Height, serialization::ZcashDeserialize as _, subtree::NoteCommitmentSubtreeIndex,
};
use zebra_rpc::{
    client::{GetAddressBalanceRequest, TransactionObject},
    methods::{GetAddressTxIdsRequest, GetBlock, GetBlockHashResponse, GetRawTransaction},
};

use zaino_address::{ValidatedAddress, ZValidatedAddress};
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo, StandardFee,
};

use zaino_proto::proto::{
    compact_formats::CompactBlock,
    service::{
        AddressList, Balance, BlockId, BlockRange, Duration, GetAddressUtxosArg,
        GetAddressUtxosReply, GetAddressUtxosReplyList, GetMempoolTxRequest, LightdInfo,
        PingResponse, RawTransaction, SendResponse, TransparentAddressBlockFilter, TreeState,
        TxFilter,
    },
    utils::{blockid_to_hashorheight, PoolTypeFilter, ValidatedBlockRangeRequest},
};

use crate::legacy::status::{Status, StatusType};
#[allow(deprecated)]
use crate::{
    chain_index::{types, validator_source::ZebraValidatorSource},
    config::{
        CommonBackendConfig, DonationAddress, NodeBackedIndexerServiceConfig,
        ValidatorConnectionType,
    },
    error::NodeBackedIndexerServiceError,
    indexer::{IndexerSubscriber, LightWalletIndexer, ZcashIndexer, ZcashService},
    stream::{
        AddressStream, CompactBlockStream, CompactTransactionStream, RawTransactionStream,
        UtxoReplyStream,
    },
    utils::{get_build_info, ServiceMetadata},
};
use crate::{
    ChainIndex, ChainIndexRpcExt, ChainIndexSourcePorts, NodeBackedChainIndex,
    NodeBackedChainIndexSubscriber,
};
use zaino_service::ServiceSource;
use zaino_source_zebra::ZebraValidator;

/// A single node-backed chain-fetch + tx-submission service, generic over its
/// `zaino-source` validator.
///
/// The JSON-RPC (`Rpc`) and direct-`ReadStateService` (`Direct`) backends are one
/// type selected at runtime via [`NodeBackedIndexerServiceConfig`]. Production
/// instantiates the default `V = ZebraValidator`; tests may use a mock validator.
///
/// This service is a central service — create a [`NodeBackedIndexerServiceSubscriber`]
/// (via [`ZcashService::get_subscriber`]) to fetch data. This lets large numbers of
/// concurrent subscribers run without contending on the single central process.
///
/// NOTE: We do not implement `Clone` for the central service: it owns and closes its
/// child processes. Subscribers are the clone-safe read handles.
pub struct NodeBackedIndexerService<V: ChainIndexSourcePorts + ServiceSource = ZebraValidator> {
    /// Core indexer.
    indexer: NodeBackedChainIndex<V>,
    /// Service metadata.
    data: ServiceMetadata,
    /// Connection-independent config (validator RPC, storage, network, ...).
    config: CommonBackendConfig,
}

impl<V: ChainIndexSourcePorts + ServiceSource> Status for NodeBackedIndexerService<V> {
    fn status(&self) -> StatusType {
        self.indexer.status()
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedIndexerService<V> {
    /// Tears down the indexer (sync loop, finalised DB, mempool, and any source-owned
    /// syncer task) from a synchronous context. Shared by [`ZcashService::close`] and
    /// [`Drop`].
    ///
    /// `block_in_place` is only legal on a multi-thread runtime; on a
    /// current-thread runtime or a thread with no runtime it panics, and a
    /// panic inside `Drop` during unwind aborts the process. Those contexts
    /// fall back to the synchronous best-effort teardown, which skips only
    /// the finalised DB's async shutdown step (the DB's own `Drop` still
    /// releases its resources).
    fn shutdown_blocking(&self) {
        use tokio::runtime::{Handle, RuntimeFlavor};

        match Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| {
                    handle.block_on(async {
                        if let Err(error) = self.indexer.shutdown().await {
                            warn!(%error, "chain index shutdown failed");
                        }
                    });
                });
            }
            _ => self.indexer.shutdown_sync_best_effort(),
        }
    }
}

impl ZcashService for NodeBackedIndexerService<ZebraValidator> {
    type Subscriber = NodeBackedIndexerServiceSubscriber<ZebraValidator>;
    type Config = NodeBackedIndexerServiceConfig;

    fn finalised_state_mode(&self) -> crate::FinalisedStateMode {
        self.indexer.finalised_state_mode()
    }

    /// Initializes a new [`NodeBackedIndexerService`] and starts its sync process.
    #[instrument(name = "NodeBackedIndexerService::spawn", skip(config), fields(network = %config.common.network))]
    async fn spawn(
        config: NodeBackedIndexerServiceConfig,
    ) -> Result<Self, NodeBackedIndexerServiceError> {
        info!(
            rpc_address = %config.common.validator_rpc_address,
            network = %config.common.network,
            "Launching NodeBackedIndexerService"
        );

        // Select the validator connection from config; both arms return the built source,
        // the validator `getinfo` used for service metadata, and the runtime network
        // (activation schedule adopted from the validator at first contact, zaino#1076).
        let (source, zebra_build_data, network) = match &config.connection {
            ValidatorConnectionType::Rpc => ZebraValidatorSource::spawn_rpc(&config.common).await,
            ValidatorConnectionType::Direct(direct) => {
                ZebraValidatorSource::spawn_direct(&config.common, direct).await
            }
        }
        .map_err(|error| NodeBackedIndexerServiceError::Critical(error.to_string()))?;

        let data = ServiceMetadata::new(
            get_build_info(config.common.indexer_version.clone()),
            network.clone(),
            zebra_build_data.build,
            zebra_build_data.subversion,
        );
        info!(build = %data.zebra_build(), subversion = %data.zebra_subversion(), "Connected to Zcash node");

        #[allow(deprecated)]
        let indexer = NodeBackedChainIndex::new(
            source,
            crate::config::ChainIndexConfig::from_backend_config(&config.common, network),
        )
        .await
        .map_err(|error| NodeBackedIndexerServiceError::Critical(error.to_string()))?;
        // the head at the validator's tip before serving; never the store's build
        indexer
            .wait_for_head()
            .await
            .map_err(|error| NodeBackedIndexerServiceError::Critical(error.to_string()))?;

        Ok(Self {
            indexer,
            data,
            config: config.common,
        })
    }

    /// Returns a [`NodeBackedIndexerServiceSubscriber`].
    fn get_subscriber(
        &self,
    ) -> IndexerSubscriber<NodeBackedIndexerServiceSubscriber<ZebraValidator>> {
        IndexerSubscriber::new(NodeBackedIndexerServiceSubscriber {
            indexer: self.indexer.subscriber(),
            data: self.data.clone(),
            config: self.config.clone(),
        })
    }

    /// Shuts down the service, tearing down its indexer (sync loop, finalised DB,
    /// mempool, and any source-owned syncer task).
    fn close(&mut self) {
        self.shutdown_blocking();
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> Drop for NodeBackedIndexerService<V> {
    fn drop(&mut self) {
        self.shutdown_blocking();
    }
}

/// A clone-safe, read-only subscriber to a [`NodeBackedIndexerService`].
#[derive(Debug)]
pub struct NodeBackedIndexerServiceSubscriber<
    V: ChainIndexSourcePorts + ServiceSource = ZebraValidator,
> {
    /// Core indexer.
    pub indexer: NodeBackedChainIndexSubscriber<V>,
    /// Service metadata.
    pub data: ServiceMetadata,
    /// Connection-independent config (validator RPC, storage, network, ...).
    config: CommonBackendConfig,
}

/// Written out: the validator is shared, so `V` need not be `Clone`.
impl<V: ChainIndexSourcePorts + ServiceSource> Clone for NodeBackedIndexerServiceSubscriber<V> {
    fn clone(&self) -> Self {
        Self {
            indexer: self.indexer.clone(),
            data: self.data.clone(),
            config: self.config.clone(),
        }
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> Status for NodeBackedIndexerServiceSubscriber<V> {
    fn status(&self) -> StatusType {
        self.indexer.status()
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> crate::IndexedTipIndexer
    for NodeBackedIndexerServiceSubscriber<V>
{
    fn subscribe_indexed_tips(&self) -> crate::IndexedTipStream {
        self.indexer.indexed_tip_stream()
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedIndexerServiceSubscriber<V> {
    /// Fetches the current status
    #[deprecated(note = "Use the Status trait method instead")]
    pub fn get_status(&self) -> StatusType {
        self.indexer.status()
    }

    /// Returns the runtime network, carrying the activation schedule
    /// adopted from the validator (zaino#1076).
    pub fn network(&self) -> zebra_chain::parameters::Network {
        self.data.network()
    }
}

/// A `getrawtransaction` txid, rejected as the legacy RPC rejects it.
#[allow(deprecated)]
fn parse_txid(txid_hex: &str) -> Result<types::TransactionHash, NodeBackedIndexerServiceError> {
    types::TransactionHash::from_hex(txid_hex).map_err(|error| {
        NodeBackedIndexerServiceError::RpcError(crate::error::LegacyRpcError::new(
            zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey,
            error.to_string(),
        ))
    })
}

/// The pools a compact-block read serves: the requested ones, or for a
/// nullifier read only their shielded nullifiers (D6-18).
fn served_pools(
    requested: &PoolTypeFilter,
    nullifiers_only: bool,
) -> zaino_primitives::types::PoolFilter {
    use zaino_primitives::types::{PoolFilter, ShieldedPool};
    let pools = zaino_proto::compact::pool_filter_from_wire(requested);
    match nullifiers_only {
        false => pools,
        true => ShieldedPool::ALL
            .into_iter()
            .filter(|pool| pools.includes(*pool))
            .fold(PoolFilter::none(), PoolFilter::with_pool)
            .nullifiers_only(),
    }
}

/// An error's causes, itself first.
fn causes(
    error: &NodeBackedIndexerServiceError,
) -> impl Iterator<Item = &(dyn std::error::Error + 'static)> {
    std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |error| {
        error.source()
    })
}

/// The legacy code and message a refusal carries, the validator's own for a
/// rejected submission.
fn legacy_rejection(error: &NodeBackedIndexerServiceError) -> Option<(i32, String)> {
    let rejection =
        causes(error).find_map(|cause| cause.downcast_ref::<crate::error::LegacyRpcError>())?;
    Some((
        i32::try_from(rejection.code).ok()?,
        rejection.message.clone(),
    ))
}

/// Whether the validator could not be reached (a transport failure, not an
/// answer).
fn unreachable(error: &NodeBackedIndexerServiceError) -> bool {
    causes(error).any(|cause| {
        cause
            .downcast_ref::<zaino_source::NonDomainError>()
            .is_some_and(|failure| {
                matches!(
                    failure.mode,
                    zaino_source::FailureMode::Connection | zaino_source::FailureMode::Timeout
                )
            })
    })
}

/// The fee on the wire; a zero fee, which no sane validator reports, is
/// refused, as lightwalletd refuses it.
fn nonzero_standard_fee(
    fee: &StandardFee,
) -> Result<zaino_proto::proto::service::StandardFee, tonic::Status> {
    if fee.fee_per_action == zaino_primitives::types::Zatoshis::ZERO {
        return Err(tonic::Status::internal(
            "Error: the validator reported a zero standard fee.",
        ));
    }
    Ok(crate::chain_index::wire_types::standard_fee_to_wire(fee))
}

/// The legacy RPC's answer for a transaction it cannot find.
#[allow(deprecated)]
fn no_such_transaction() -> NodeBackedIndexerServiceError {
    NodeBackedIndexerServiceError::RpcError(crate::error::LegacyRpcError::new(
        zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey,
        "No such mempool or main chain transaction",
    ))
}

/// The request's page: its `start_height` and `max_entries` (0: no bound but
/// the cap), as lightwalletd passes them to the validator (#46).
fn utxo_page(start_height: u64, max_entries: u32) -> Result<zaino_source::UtxoPage, tonic::Status> {
    let start = u32::try_from(start_height)
        .ok()
        .and_then(|height| zaino_primitives::types::Height::try_from(height).ok())
        .ok_or_else(|| tonic::Status::invalid_argument("Error: Start height out of range."))?;
    let max_entries = usize::try_from(max_entries)
        .map_err(|_| tonic::Status::invalid_argument("Error: Max entries out of range."))?;
    zaino_source::UtxoPage::new(start, max_entries)
        .map_err(|error| tonic::Status::invalid_argument(format!("Error: {error}.")))
}

/// A UTXO as the light-wallet protocol's reply.
fn utxo_reply(utxo: zaino_primitives::types::Utxo) -> Result<GetAddressUtxosReply, tonic::Status> {
    let zaino_primitives::types::Utxo {
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

/// Placeholder metadata and config for test-only service construction. Takes the
/// runtime (zebra) network directly — tests are their own chain, so they stand in
/// for the validator the production path adopts the schedule from.
#[cfg(test)]
fn test_service_parts(
    network: zebra_chain::parameters::Network,
) -> (ServiceMetadata, CommonBackendConfig) {
    let network_kind = zaino_primitives::types::Network::from_zebra(&network);
    (
        ServiceMetadata::new(
            get_build_info("test".to_string()),
            network,
            "test-build".to_string(),
            "test-subversion".to_string(),
        ),
        CommonBackendConfig::new(
            "127.0.0.1:0".to_string(),
            None,
            None,
            None,
            crate::legacy::config::ServiceConfig::default(),
            crate::legacy::config::StorageConfig::default(),
            true,
            network_kind,
            None,
        ),
    )
}

#[cfg(test)]
impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedIndexerService<V> {
    /// Wraps a chain index in a service for tests, with placeholder
    /// metadata/config. Lets unit tests exercise the service lifecycle over a
    /// mock source (no real validator). Production builds go through
    /// [`ZcashService::spawn`].
    pub(crate) fn new_for_test(
        indexer: NodeBackedChainIndex<V>,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        let (data, config) = test_service_parts(network);
        Self {
            data,
            config,
            indexer,
        }
    }
}

#[cfg(test)]
impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedIndexerServiceSubscriber<V> {
    /// Wraps a chain-index subscriber in a service subscriber for tests, with placeholder
    /// metadata/config. Lets unit tests drive the service RPC layer over a mock source
    /// (no real validator). Production builds go through [`ZcashService::get_subscriber`].
    pub(crate) fn new_for_test(
        indexer: NodeBackedChainIndexSubscriber<V>,
        network: zebra_chain::parameters::Network,
    ) -> Self {
        let (data, config) = test_service_parts(network);
        Self {
            data,
            config,
            indexer,
        }
    }
}

/// A subscriber to any chaintip updates.
#[derive(Clone)]
pub struct ChainTipSubscriber {
    monitor: zebra_state::ChainTipChange,
}

impl ChainTipSubscriber {
    /// Waits until the tip hash has changed (relative to the last time this method
    /// was called), then returns the best tip's block hash.
    pub async fn next_tip_hash(
        &mut self,
    ) -> Result<zebra_chain::block::Hash, tokio::sync::watch::error::RecvError> {
        self.monitor
            .wait_for_tip_change()
            .await
            .map(|tip| tip.best_tip_hash())
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedIndexerServiceSubscriber<V> {
    /// A subscriber to chain-tip updates, when the backing source exposes a
    /// local tip-change stream. `Some` only on the `Direct` connection; the
    /// `Rpc` connection (and any other stream-less source) observes tips by
    /// polling the validator and yields `None`.
    pub fn chaintip_update_subscriber(&self) -> Option<ChainTipSubscriber> {
        Some(ChainTipSubscriber {
            monitor: self.indexer.source().chain_tip_change()?,
        })
    }

    /// The page of unspent outputs a `GetAddressUtxos` request asks for.
    async fn requested_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, NodeBackedIndexerServiceError> {
        let status = NodeBackedIndexerServiceError::TonicStatusError;
        super::validate_utxo_address_count(request.addresses.len()).map_err(status)?;
        let page = utxo_page(request.start_height, request.max_entries).map_err(status)?;
        let snapshot = self.indexer.snapshot_nonfinalized_state();
        Ok(self
            .indexer
            .address_utxos_in(
                &snapshot,
                GetAddressBalanceRequest::new(request.addresses),
                page,
            )
            .await?)
    }

    /// A transaction as the light-wallet `RawTransaction`: its best-chain
    /// height, `u64::MAX` if mined only on a non-best chain, or 0 in the mempool.
    async fn light_wallet_transaction(
        &self,
        snapshot: &crate::chain_index::chain_view::ChainIndexSnapshot<V>,
        txid_hex: &str,
    ) -> Result<RawTransaction, NodeBackedIndexerServiceError> {
        let txid = parse_txid(txid_hex)?;
        let (bytes, _branch_id) = self
            .indexer
            .get_raw_transaction(snapshot, &txid)
            .await?
            .ok_or_else(|| {
                NodeBackedIndexerServiceError::TonicStatusError(tonic::Status::not_found(format!(
                    "transaction not found: {txid_hex}"
                )))
            })?;
        let (best, non_best) = self.indexer.get_transaction_status(snapshot, &txid).await?;
        let height = match best {
            Some(types::BestChainLocation::Block(_, height)) => u64::from(height.0),
            _ if non_best
                .iter()
                .any(|location| matches!(location, types::NonBestChainLocation::Block(..))) =>
            {
                u64::MAX
            }
            _ => 0,
        };
        Ok(RawTransaction {
            data: bytes::Bytes::from(bytes),
            height,
        })
    }

    /// Shared body of `get_block` and `get_block_nullifiers`.
    async fn compact_block_by_id(
        &self,
        request: BlockId,
        nullifiers_only: bool,
    ) -> Result<CompactBlock, NodeBackedIndexerServiceError> {
        let status = NodeBackedIndexerServiceError::TonicStatusError;
        let hash_or_height =
            blockid_to_hashorheight(request).ok_or(status(tonic::Status::invalid_argument(
                "Error: Invalid hash and/or height out of range. Failed to convert to u32.",
            )))?;

        let snapshot = self.indexer.snapshot_nonfinalized_state();
        let height = match hash_or_height {
            HashOrHeight::Height(height) => height.0,
            HashOrHeight::Hash(hash) => {
                match self.indexer.get_block_height(&snapshot, hash.into()).await {
                    Ok(Some(height)) => height.0,
                    Ok(None) => {
                        return Err(status(tonic::Status::invalid_argument(
                            "Error: Invalid hash and/or height out of range. Hash not found in chain",
                        )));
                    }
                    Err(error) => {
                        return Err(NodeBackedIndexerServiceError::ChainIndexError(error))
                    }
                }
            }
        };

        // `BlockID` has no `poolTypes`: one block is the whole block, every
        // pool and every transaction, as lightwalletd serves it (#18, #19);
        // a nullifier read keeps each transaction with only its nullifiers.
        let every_pool = PoolTypeFilter::includes_all();
        let pools = served_pools(&every_pool, nullifiers_only);
        let block = self
            .indexer
            .compact_block(&snapshot, types::Height(height), pools, true)
            .await;
        let failure = match block {
            Ok(Some(block)) => return Ok(block),
            Ok(None) => status(tonic::Status::not_found(
                "Error: Failed to retrieve block from state.",
            )),
            Err(e) => NodeBackedIndexerServiceError::from(e),
        };

        let chain_height = u32::from(snapshot.best_tip().height);
        let sapling_activation = self.data.network().sapling_activation_height();
        Err(match hash_or_height {
            HashOrHeight::Height(Height(height)) if height > chain_height => {
                status(tonic::Status::out_of_range(format!(
                    "Error: Height out of range [{hash_or_height}]. Height requested \
                    is greater than the best chain tip [{chain_height}].",
                )))
            }
            HashOrHeight::Height(height) if nullifiers_only && height < sapling_activation => {
                status(tonic::Status::out_of_range(format!(
                    "Error: Height out of range [{hash_or_height}]. Height requested \
                    is below sapling activation height [{}].",
                    sapling_activation.0,
                )))
            }
            _otherwise => failure,
        })
    }

    /// Shared body of `get_block_range` and `get_block_range_nullifiers`: streams the
    /// requested compact-block range through a channel, each block read with only its
    /// nullifiers when `nullifiers_only`. `rpc_name` labels log lines and nothing else.
    #[allow(deprecated)]
    async fn spawn_block_range_stream(
        &self,
        request: &BlockRange,
        rpc_name: &'static str,
        nullifiers_only: bool,
    ) -> Result<CompactBlockStream, NodeBackedIndexerServiceError> {
        let validated_request = ValidatedBlockRangeRequest::new_from_block_range(request)
            .map_err(NodeBackedIndexerServiceError::from)?;

        let pools = served_pools(validated_request.pool_type_filter(), nullifiers_only);

        let start = validated_request.start();
        let end = validated_request.end();

        let indexer = self.indexer.clone();
        let snapshot = indexer.snapshot_nonfinalized_state();
        let chain_height = u32::from(snapshot.best_tip().height);

        Ok(CompactBlockStream::new(super::spawn_timed_stream(
            self.timeout_channel_size(),
            4,
            tonic::Status::deadline_exceeded(format!("Error: {rpc_name} gRPC request timed out.")),
            |channel_tx| async move {
                let stream = indexer.compact_block_stream(
                    &snapshot,
                    types::Height(start),
                    types::Height(end),
                    pools,
                );
                let failure = match stream {
                    Ok(Some(mut compact_block_stream)) => {
                        while let Some(stream_item) = compact_block_stream.next().await {
                            if channel_tx.send(stream_item).await.is_err() {
                                return;
                            }
                        }
                        return;
                    }
                    Err(_) | Ok(None) if start > chain_height || end > chain_height => {
                        let offending_height = if start > chain_height { start } else { end };
                        tonic::Status::out_of_range(format!(
                            "Error: Height out of range [{offending_height}]. Height requested \
                            is greater than the best chain tip [{chain_height}].",
                        ))
                    }
                    Ok(None) => tonic::Status::not_found("Error: Failed to retrieve blocks."),
                    Err(e) => NodeBackedIndexerServiceError::from(e).into(),
                };
                if let Err(e) = channel_tx.send(Err(failure)).await {
                    warn!(%e, "{rpc_name} channel closed unexpectedly");
                }
            },
        )))
    }
}

/// Methods available only on the production (`ZebraValidatorSource`-backed) subscriber:
/// the `Direct` connection owns a `ReadStateService` that the generic source
/// abstraction does not expose.
impl NodeBackedIndexerServiceSubscriber<ZebraValidator> {
    /// The backing Zebra [`zebra_state::ReadStateService`]; `None` over an RPC
    /// connection.
    ///
    /// Test-only escape hatch: live tests recompute expected chain data directly off the
    /// `ReadStateService`. Production code goes through the `ChainIndex` API.
    #[cfg(feature = "test_dependencies")]
    pub fn read_state_service(&self) -> Option<zebra_state::ReadStateService> {
        self.indexer.source().read_state_service().cloned()
    }

    /// The indexer's mempool subscriber.
    ///
    /// Test-only escape hatch: live tests recompute expected `getmempoolinfo` values
    /// directly off the mempool's entries. Production code goes through the `ChainIndex`
    /// mempool API.
    #[cfg(feature = "test_dependencies")]
    pub fn mempool(&self) -> zaino_mempool_service::MempoolSubscriber {
        self.indexer.mempool_subscriber()
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> ZcashIndexer
    for NodeBackedIndexerServiceSubscriber<V>
{
    type Error = NodeBackedIndexerServiceError;

    /// Returns information about all changes to the given transparent addresses within the given inclusive block-height range.
    ///
    /// Defaults: if start or end are not specified, they default to 0.
    ///
    /// ### Normalization
    ///
    /// - If start is greater than the latest block height (tip), start is set to the tip.
    /// - If end is 0 or greater than the tip, end is set to the tip.
    ///
    /// ### Validation
    ///
    /// If the resulting start is greater than end, the call fails with an error.
    /// (Thus, [tip, tip] is valid and returns only the tip block.)
    ///
    /// [the original legacy full-node implementation](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/misc.cpp#L881)
    ///
    /// Zcash RPC reference: [`getaddressdeltas`](https://zcash.github.io/rpc/getaddressdeltas.html)
    /// method: post
    /// tags: address
    async fn get_address_deltas(
        &self,
        params: AddressDeltasRequest,
    ) -> Result<AddressDeltas, Self::Error> {
        Ok(self.indexer.get_address_deltas(params).await?)
    }

    /// Returns software information from the RPC server, as a [`NodeInfo`] JSON struct.
    ///
    /// Zcash RPC reference: [`getinfo`](https://zcash.github.io/rpc/getinfo.html)
    /// method: post
    /// tags: control
    ///
    /// # Notes
    ///
    /// [The Zcash RPC reference](https://zcash.github.io/rpc/getinfo.html) might not show some fields
    /// in Zebra's [`NodeInfo`]. Zebra uses the field names and formats from the
    /// [the legacy full node's code](https://github.com/zcash/zcash/blob/v4.6.0-1/src/rpc/misc.cpp#L86-L87).
    fn network(&self) -> zebra_chain::parameters::Network {
        self.data.network()
    }

    async fn get_info(&self) -> Result<NodeInfo, Self::Error> {
        Ok(self.indexer.get_info().await?)
    }

    /// Returns blockchain state information, as a [`BlockchainInfo`](zaino_primitives::types::BlockchainInfo) JSON struct.
    ///
    /// Zcash RPC reference: [`getblockchaininfo`](https://zcash.github.io/rpc/getblockchaininfo.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Notes
    ///
    /// Some fields from the Zcash RPC reference are missing from Zebra's [`BlockchainInfo`](zaino_primitives::types::BlockchainInfo). It only contains the fields
    /// [required for lightwalletd support.](https://github.com/zcash/lightwalletd/blob/v0.4.9/common/common.go#L72-L89)
    async fn get_blockchain_info(
        &self,
    ) -> Result<zaino_primitives::types::BlockchainInfo, Self::Error> {
        Ok(self.indexer.get_blockchain_info().await?)
    }

    /// Returns details on the active state of the TX memory pool.
    ///
    /// online zcash rpc reference: [`getmempoolinfo`](https://zcash.github.io/rpc/getmempoolinfo.html)
    /// method: post
    /// tags: mempool
    ///
    /// Canonical source code implementation: [`getmempoolinfo`](https://github.com/zcash/zcash/blob/18238d90cd0b810f5b07d5aaa1338126aa128c06/src/rpc/blockchain.cpp#L1555)
    ///
    /// Zebra does not support this RPC call directly.
    async fn get_mempool_info(&self) -> Result<zaino_primitives::types::MempoolInfo, Self::Error> {
        Ok(self.indexer.get_mempool_info().await)
    }

    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, Self::Error> {
        Ok(self.indexer.get_peer_info().await?)
    }

    /// Returns the proof-of-work difficulty as a multiple of the minimum difficulty.
    ///
    /// Zcash RPC reference: [`getdifficulty`](https://zcash.github.io/rpc/getdifficulty.html)
    /// method: post
    /// tags: blockchain
    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        Ok(self.indexer.get_difficulty().await?)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<BlockSubsidy, Self::Error> {
        Ok(self.indexer.get_block_subsidy(height).await?)
    }

    async fn get_standard_fee(&self) -> Result<StandardFee, Self::Error> {
        Ok(self.indexer.get_standard_fee().await?)
    }

    /// Returns the total balance of a provided `addresses` in an [`AddressBalance`](zaino_primitives::types::AddressBalance) instance.
    ///
    /// Zcash RPC reference: [`getaddressbalance`](https://zcash.github.io/rpc/getaddressbalance.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `address_strings`: (object, example={"addresses": ["tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ"]}) A JSON map with a single entry
    ///     - `addresses`: (array of strings) A list of base-58 encoded addresses.
    ///
    /// # Notes
    ///
    /// the legacy full node also accepts a single string parameter instead of an array of strings, but Zebra
    /// doesn't because lightwalletd always calls this RPC with an array of addresses.
    ///
    /// the legacy full node also returns the total amount of Zatoshis received by the addresses, but Zebra
    /// doesn't because lightwalletd doesn't use that information.
    ///
    /// The RPC documentation says that the returned object has a string `balance` field, but
    /// the legacy full node actually [returns an
    /// integer](https://github.com/zcash/lightwalletd/blob/bdaac63f3ee0dbef62bde04f6817a9f90d483b00/common/common.go#L128-L130).
    async fn z_get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<zaino_primitives::types::AddressBalance, Self::Error> {
        Ok(self.indexer.get_address_balance(address_strings).await?)
    }

    /// Sends the raw bytes of a signed transaction to the local node's mempool, if the transaction is valid.
    /// Returns the [`TransactionHash`](zaino_primitives::types::TransactionHash) for the transaction, as a JSON string.
    ///
    /// Zcash RPC reference: [`sendrawtransaction`](https://zcash.github.io/rpc/sendrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `raw_transaction_hex`: (string, required, example="signedhex") The hex-encoded raw transaction bytes.
    ///
    /// # Notes
    ///
    /// the legacy full node accepts an optional `allowhighfees` parameter. Zebra doesn't support this parameter,
    /// because lightwalletd doesn't use it.
    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<zaino_primitives::types::TransactionId, Self::Error> {
        Ok(self
            .indexer
            .send_raw_transaction(raw_transaction_hex)
            .await?)
    }

    /// Returns the requested block by hash or height, as a [`GetBlock`] JSON string.
    /// If the block is not in Zebra's state, returns
    /// [error code `-8`.](https://github.com/zcash/zcash/issues/5758) if a height was
    /// passed or -5 if a hash was passed.
    ///
    /// Zcash RPC reference: [`getblock`](https://zcash.github.io/rpc/getblock.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash_or_height`: (string, required, example="1") The hash or height for the block to be returned.
    /// - `verbosity`: (number, optional, default=1, example=1) 0 for hex encoded data, 1 for a json object, and 2 for json object with transaction data.
    ///
    /// # Notes
    ///
    /// Zebra previously partially supported verbosity=1 by returning only the
    /// fields required by lightwalletd ([`lightwalletd` only reads the `tx`
    /// field of the result](https://github.com/zcash/lightwalletd/blob/dfac02093d85fb31fb9a8475b884dd6abca966c7/common/common.go#L152)).
    /// That verbosity level was migrated to "3"; so while lightwalletd will
    /// still work by using verbosity=1, it will sync faster if it is changed to
    /// use verbosity=3.
    ///
    /// The undocumented `chainwork` field is not returned.
    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        Ok(self.indexer.z_get_block(hash_or_height, verbosity).await?)
    }

    /// Returns information about the given block and its transactions.
    ///
    /// Zcash RPC reference: [`getblockdeltas`](https://zcash.github.io/rpc/getblockdeltas.html)
    /// method: post
    /// tags: blockchain
    ///
    /// Note: This method has only been implemented in the legacy full node. Zebra has no intention of supporting it.
    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, Self::Error> {
        Ok(self.indexer.get_block_deltas(hash).await?)
    }

    async fn get_block_header(&self, hash: String) -> Result<BlockHeaderVerbose, Self::Error> {
        Ok(self.indexer.get_block_header(hash).await?)
    }

    async fn get_raw_block_header(&self, hash: String) -> Result<Vec<u8>, Self::Error> {
        Ok(self.indexer.get_raw_block_header(hash).await?)
    }

    async fn get_mining_info(&self) -> Result<MiningInfo, Self::Error> {
        Ok(self.indexer.get_mining_info().await?)
    }

    /// Returns statistics about the unspent transaction output set.
    ///
    /// Zcash RPC reference: [`gettxoutsetinfo`](https://zcash.github.io/rpc/gettxoutsetinfo.html)
    /// method: post
    /// tags: blockchain
    async fn get_tx_out_set_info(
        &self,
    ) -> Result<zaino_primitives::types::TxOutSetInfo, Self::Error> {
        Ok(self.indexer.get_tx_out_set_info().await?)
    }

    /// Returns the hash of the best block (tip) of the longest chain.
    /// online Zcash RPC reference: [`getbestblockhash`](https://zcash.github.io/rpc/getbestblockhash.html)
    /// The legacy full-node doc reference above says there are no parameters and the result is a "hex" (string) of the block hash hex encoded.
    /// method: post
    /// tags: blockchain
    /// Return the hex encoded hash of the best (tip) block, in the longest block chain.
    ///
    /// # Parameters
    ///
    /// No request parameters.
    ///
    /// # Notes
    ///
    /// Return should be valid hex encoded.
    ///
    /// The Zcash source code is considered canonical:
    /// [In the rpc definition](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/common.h#L48) there are no required params, or optional params.
    /// [The function in rpc/blockchain.cpp](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L325)
    /// where `return chainActive.Tip()->GetBlockHash().GetHex();` is the [return expression](https://github.com/zcash/zcash/blob/654a8be2274aa98144c80c1ac459400eaf0eacbe/src/rpc/blockchain.cpp#L339)returning a `std::string`
    async fn get_best_blockhash(&self) -> Result<GetBlockHashResponse, Self::Error> {
        let tip = self.indexer.snapshot_nonfinalized_state().best_tip();
        Ok(GetBlockHashResponse::new(zebra_chain::block::Hash(
            tip.hash.into(),
        )))
    }

    /// Returns the current block count in the best valid block chain.
    ///
    /// Zcash RPC reference: [`getblockcount`](https://zcash.github.io/rpc/getblockcount.html)
    /// method: post
    /// tags: blockchain
    async fn get_block_count(&self) -> Result<Height, Self::Error> {
        self.chain_height().await
    }

    #[allow(deprecated)]
    async fn get_chain_tips(
        &self,
    ) -> Result<Vec<zaino_primitives::types::rpc::ChainTip>, Self::Error> {
        Ok(self.indexer.snapshot_nonfinalized_state().chain_tips())
    }

    /// Return information about the given Zcash address.
    ///
    /// # Parameters
    /// - `address`: (string, required, example="tmHMBeeYRuc2eVicLNfP15YLxbQsooCA6jb") The Zcash transparent address to validate.
    ///
    /// Zcash RPC reference: [`validateaddress`](https://zcash.github.io/rpc/validateaddress.html)
    /// method: post
    /// tags: blockchain
    async fn validate_address(&self, address: String) -> Result<ValidatedAddress, Self::Error> {
        #[allow(deprecated)]
        let network = self.data.network();
        Ok(zaino_address::validate_address(address, &network))
    }

    #[allow(deprecated)]
    async fn z_validate_address(&self, address: String) -> Result<ZValidatedAddress, Self::Error> {
        #[allow(deprecated)]
        let network = self.data.network();
        Ok(zaino_address::z_validate_address(address, &network))
    }

    /// Returns all transaction ids in the memory pool, as a JSON array.
    ///
    /// Zcash RPC reference: [`getrawmempool`](https://zcash.github.io/rpc/getrawmempool.html)
    /// method: post
    /// tags: blockchain
    async fn get_raw_mempool(&self) -> Result<Vec<String>, Self::Error> {
        // Ok(self.fetcher.get_raw_mempool().await?.transactions)
        Ok(self
            .indexer
            .get_mempool_txids()
            .await?
            .iter()
            .map(|txid| txid.to_rpc_hex())
            .collect())
    }

    /// Returns information about the given block's Sapling & Orchard tree state.
    ///
    /// Zcash RPC reference: [`z_gettreestate`](https://zcash.github.io/rpc/z_gettreestate.html)
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `hash | height`: (string, required, example="00000000febc373a1da2bd9f887b105ad79ddc26ac26c2b28652d64e5207c5b5") The block hash or height.
    ///
    /// # Notes
    ///
    /// The legacy full-node doc reference above says that the parameter "`height` can be
    /// negative where -1 is the last known valid block". On the other hand,
    /// `lightwalletd` only uses positive heights, so Zebra does not support
    /// negative heights.
    // `NodeBackedIndexerServiceError` is large enough to trip `result_large_err`.
    #[allow(clippy::result_large_err)]
    async fn z_get_treestate(
        &self,
        hash_or_height: String,
    ) -> Result<zaino_primitives::types::Treestate, Self::Error> {
        // ChainView's answer; a block it does not hold is zebrad's −8
        Ok(self.indexer.get_treestate_by_id(hash_or_height).await?)
    }

    /// Returns information about a range of Sapling, Orchard, or Ironwood subtrees.
    ///
    /// Zcash RPC reference: [`z_getsubtreesbyindex`](https://zcash.github.io/rpc/z_getsubtreesbyindex.html) - TODO: fix link
    /// method: post
    /// tags: blockchain
    ///
    /// # Parameters
    ///
    /// - `pool`: (string, required) The pool from which subtrees should be returned. Either "sapling", "orchard", or "ironwood".
    /// - `start_index`: (number, required) The index of the first 2^16-leaf subtree to return.
    /// - `limit`: (number, optional) The maximum number of subtree values to return.
    ///
    /// # Notes
    ///
    /// While Zebra is doing its initial subtree index rebuild, subtrees will become available
    /// starting at the chain tip. This RPC will return an empty list if the `start_index` subtree
    /// exists, but has not been rebuilt yet. This matches the legacy full node's behaviour when subtrees aren't
    /// available yet. (But the legacy full node does its rebuild before syncing any blocks.)
    #[allow(deprecated)]
    async fn z_get_subtrees_by_index(
        &self,
        pool: zaino_primitives::types::ShieldedPool,
        start_index: NoteCommitmentSubtreeIndex,
        limit: Option<NoteCommitmentSubtreeIndex>,
    ) -> Result<zaino_primitives::types::rpc::SubtreeRoots, Self::Error> {
        let subtrees = zaino_chain::TreestateRead::subtree_roots(
            &self.indexer.snapshot_nonfinalized_state(),
            pool,
            start_index.0,
            limit.map(|index| index.0),
        )
        .await
        .map_err(crate::error::ChainIndexError::from)?;

        Ok(zaino_primitives::types::rpc::SubtreeRoots {
            pool,
            start_index: start_index.0,
            subtrees,
        })
    }

    /// Returns the raw transaction data, as a [`GetRawTransaction`] JSON string or structure.
    ///
    /// Zcash RPC reference: [`getrawtransaction`](https://zcash.github.io/rpc/getrawtransaction.html)
    /// method: post
    /// tags: transaction
    ///
    /// # Parameters
    ///
    /// - `txid`: (string, required, example="mytxid") The transaction ID of the transaction to be returned.
    /// - `verbose`: (number, optional, default=0, example=1) If 0, return a string of hex-encoded data, otherwise return a JSON object.
    ///
    /// # Notes
    ///
    /// We don't currently support the `blockhash` parameter since lightwalletd does not
    /// use it.
    async fn get_raw_transaction(
        &self,
        txid_hex: String,
        verbose: Option<u8>,
    ) -> Result<GetRawTransaction, Self::Error> {
        let txid = parse_txid(&txid_hex)?;
        let snapshot = self.indexer.snapshot_nonfinalized_state();

        let Some((serialized_transaction, _consensus_branch_id)) =
            self.indexer.get_raw_transaction(&snapshot, &txid).await?
        else {
            return Err(no_such_transaction());
        };

        if verbose.unwrap_or(0) == 0 {
            return Ok(GetRawTransaction::Raw(
                zebra_chain::transaction::SerializedTransaction::from(serialized_transaction),
            ));
        }

        let transaction = zebra_chain::transaction::Transaction::zcash_deserialize(
            serialized_transaction.as_slice(),
        )?;

        let (best_chain_location, non_best_chain_locations) = self
            .indexer
            .get_transaction_status(&snapshot, &txid)
            .await?;

        // The zebra sink still takes the RPC integers, so the typed state is
        // rendered through its codec at this boundary; the constructor owns
        // the depth + 1 off-by-one the arithmetic here used to fold in.
        let out_of_range = |e: zaino_primitives::types::HeightOverflow| {
            NodeBackedIndexerServiceError::TonicStatusError(tonic::Status::internal(format!(
                "best-chain height out of range: {e}"
            )))
        };
        // `time`/`blocktime` are the containing block's header timestamp, which the
        // index already holds — no validator round-trip. An unmined transaction has
        // no containing block, so both fields stay absent, as zebrad leaves them.
        let (height, confirmations, block_hash, block_time, in_best_chain) =
            match best_chain_location {
                Some(types::BestChainLocation::Block(block_hash, height)) => {
                    let confirmations = zaino_primitives::types::TxConfirmations::Mined(
                        zaino_primitives::types::BlockConfirmations::of_best_chain_block(
                            zaino_primitives::types::Height::try_from(height.0)
                                .map_err(out_of_range)?,
                            zaino_primitives::types::Height::try_from(u32::from(
                                snapshot.best_tip().height,
                            ))
                            .map_err(out_of_range)?,
                        ),
                    );

                    let block_time = zaino_chain::BlockRead::block_header(
                        &snapshot,
                        zaino_chain::BlockId::Hash(types::domain_hash(block_hash)),
                    )
                    .await
                    .map_err(crate::error::ChainIndexError::from)?
                    .and_then(|header| chrono::DateTime::from_timestamp(i64::from(header.time), 0));

                    (
                        Some(zebra_chain::block::Height::from(height)),
                        Some(confirmations.to_rpc_i64()),
                        Some(zebra_chain::block::Hash::from(block_hash)),
                        block_time,
                        Some(confirmations.is_in_best_chain()),
                    )
                }
                // as zebrad (#39): a mempool transaction has no confirmations
                // key; one mined only on a side chain names that block
                Some(types::BestChainLocation::Mempool(_height)) => {
                    (None, None, None, None, Some(false))
                }
                None => {
                    let side_block =
                        non_best_chain_locations
                            .iter()
                            .find_map(|location| match location {
                                types::NonBestChainLocation::Block(hash, _) => {
                                    Some(zebra_chain::block::Hash::from(*hash))
                                }
                                types::NonBestChainLocation::Mempool(_) => None,
                            });
                    (None, None, side_block, None, Some(false))
                }
            };

        Ok(GetRawTransaction::Object(Box::new(
            TransactionObject::from_transaction(
                transaction.into(),
                height,
                confirmations,
                #[allow(deprecated)]
                &self.data.network(),
                block_time,
                block_hash,
                in_best_chain,
                zebra_chain::transaction::Hash::from(txid),
            ),
        )))
    }

    /// Returns details about an unspent transaction output.
    ///
    /// Zcash RPC reference: [`gettxout`](https://zcash.github.io/rpc/gettxout.html)
    /// method: post
    /// tags: transaction
    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, Self::Error> {
        Ok(self.indexer.get_tx_out(txid, n, include_mempool).await?)
    }

    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<zaino_service::Spend, Self::Error> {
        Ok(self.indexer.get_spent_info(outpoint).await?)
    }

    async fn chain_height(&self) -> Result<Height, Self::Error> {
        Ok(Height(u32::from(
            self.indexer.snapshot_nonfinalized_state().best_tip().height,
        )))
    }
    /// Returns the transaction ids made by the provided transparent addresses.
    ///
    /// Zcash RPC reference: [`getaddresstxids`](https://zcash.github.io/rpc/getaddresstxids.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `request`: (object, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"], \"start\": 1000, \"end\": 2000}) A struct with the following named fields:
    ///     - `addresses`: (json array of string, required) The addresses to get transactions from.
    ///     - `start`: (numeric, required) The lower height to start looking for transactions (inclusive).
    ///     - `end`: (numeric, required) The top height to stop looking for transactions (inclusive).
    ///
    /// # Notes
    ///
    /// Only the multi-argument format is used by lightwalletd and this is what we currently support:
    /// <https://github.com/zcash/lightwalletd/blob/631bb16404e3d8b045e74a7c5489db626790b2f6/common/common.go#L97-L102>
    async fn get_address_tx_ids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<String>, Self::Error> {
        Ok(self
            .indexer
            .get_address_txids(request)
            .await?
            .into_iter()
            .map(|transaction_hash| transaction_hash.to_rpc_hex())
            .collect())
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Zcash RPC reference: [`getaddressutxos`](https://zcash.github.io/rpc/getaddressutxos.html)
    /// method: post
    /// tags: address
    ///
    /// # Parameters
    ///
    /// - `addresses`: (array, required, example={\"addresses\": [\"tmYXBYJj1K7vhejSec5osXK2QsGa5MTisUQ\"]}) The addresses to get outputs from.
    ///
    /// # Notes
    ///
    /// lightwalletd always uses the multi-address request, without chaininfo:
    /// <https://github.com/zcash/lightwalletd/blob/master/frontend/service.go#L402>
    async fn z_get_address_utxos(
        &self,
        addresses: GetAddressBalanceRequest,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, Self::Error> {
        Ok(self.indexer.get_address_utxos(addresses).await?)
    }

    /// Returns the estimated network solutions per second based on the last n blocks.
    ///
    /// Zcash RPC reference: [`getnetworksolps`](https://zcash.github.io/rpc/getnetworksolps.html)
    /// method: post
    /// tags: blockchain
    ///
    /// This RPC is implemented in the [mining.cpp](https://github.com/zcash/zcash/blob/d00fc6f4365048339c83f463874e4d6c240b63af/src/rpc/mining.cpp#L104)
    /// file of the Zcash repository. The Zebra implementation can be found [here](https://github.com/ZcashFoundation/zebra/blob/19bca3f1159f9cb9344c9944f7e1cb8d6a82a07f/zebra-rpc/src/methods.rs#L2687).
    ///
    /// # Parameters
    ///
    /// - `blocks`: (number, optional, default=120) Number of blocks, or -1 for blocks over difficulty averaging window.
    /// - `height`: (number, optional, default=-1) To estimate network speed at the time of a specific block height.
    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<u64, Self::Error> {
        Ok(self.indexer.get_network_sol_ps(blocks, height).await?)
    }
}

#[allow(deprecated)]
impl<V: ChainIndexSourcePorts + ServiceSource> LightWalletIndexer
    for NodeBackedIndexerServiceSubscriber<V>
{
    /// Return the height of the tip of the best chain
    async fn get_latest_block(&self) -> Result<BlockId, Self::Error> {
        Ok(crate::chain_index::wire_types::block_index_to_wire(
            &types::block_index(self.indexer.snapshot_nonfinalized_state().best_tip()),
        ))
    }

    /// Return the compact block corresponding to the given block identifier
    async fn get_block(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        self.compact_block_by_id(request, false).await
    }

    /// Same as GetBlock, carrying only the shielded nullifiers (Sapling spends,
    /// Orchard and Ironwood actions) and zero tree sizes.
    async fn get_block_nullifiers(&self, request: BlockId) -> Result<CompactBlock, Self::Error> {
        self.compact_block_by_id(request, true).await
    }

    /// Return a list of consecutive compact blocks
    #[allow(deprecated)]
    async fn get_block_range(
        &self,
        request: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error> {
        self.spawn_block_range_stream(&request, "GetBlockRange", false)
            .await
    }

    /// Same as GetBlockRange, carrying only the shielded nullifiers (Sapling
    /// spends, Orchard and Ironwood actions) and zero tree sizes.
    #[allow(deprecated)]
    async fn get_block_range_nullifiers(
        &self,
        request: BlockRange,
    ) -> Result<CompactBlockStream, Self::Error> {
        self.spawn_block_range_stream(&request, "GetBlockRangeNullifiers", true)
            .await
    }

    /// Return the requested full (not compact) transaction (as from the legacy full node)
    async fn get_transaction(&self, request: TxFilter) -> Result<RawTransaction, Self::Error> {
        let hash = request.hash;
        if hash.len() != 32 {
            return Err(NodeBackedIndexerServiceError::TonicStatusError(
                tonic::Status::invalid_argument("Error: Transaction hash incorrect"),
            ));
        }
        let hash_hex = hex::encode(hash.iter().rev().copied().collect::<Vec<u8>>());
        let snapshot = self.indexer.snapshot_nonfinalized_state();
        self.light_wallet_transaction(&snapshot, &hash_hex).await
    }

    /// Submit the given transaction to the Zcash network
    async fn send_transaction(&self, request: RawTransaction) -> Result<SendResponse, Self::Error> {
        // as lightwalletd (#38): the quoted txid on success; a rejection is an
        // OK reply carrying the validator's code and message; unreachable is
        // `UNAVAILABLE`
        match self.send_raw_transaction(hex::encode(request.data)).await {
            Ok(txid) => Ok(SendResponse {
                error_code: 0,
                error_message: format!("\"{txid}\""),
            }),
            Err(error) => match (legacy_rejection(&error), unreachable(&error)) {
                (Some((error_code, error_message)), _) => Ok(SendResponse {
                    error_code,
                    error_message,
                }),
                (None, true) => Err(NodeBackedIndexerServiceError::TonicStatusError(
                    tonic::Status::unavailable(error.to_string()),
                )),
                (None, false) => Err(error),
            },
        }
    }

    // Return the transactions corresponding to the given t-address within the given block range
    async fn get_taddress_transactions(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        // one snapshot for the ids and the transactions (#49)
        let request = super::taddress_txids_request(request)
            .map_err(NodeBackedIndexerServiceError::TonicStatusError)?;
        let snapshot = self.indexer.snapshot_nonfinalized_state();
        let txids: Vec<String> = self
            .indexer
            .address_txids_in(&snapshot, request)
            .await?
            .into_iter()
            .map(|txid| txid.to_rpc_hex())
            .collect();
        let service = self.clone();
        Ok(RawTransactionStream::new(super::spawn_timed_stream(
            self.timeout_channel_size(),
            4,
            tonic::Status::internal("Error: get_taddress_transactions gRPC request timed out"),
            |transmitter| async move {
                for txid in txids {
                    let transaction = service
                        .light_wallet_transaction(&snapshot, &txid)
                        .await
                        .map_err(tonic::Status::from);
                    if transmitter.send(transaction).await.is_err() {
                        break;
                    }
                }
            },
        )))
    }

    /// Return the txids corresponding to the given t-address within the given block range
    /// this function is deprecated: use `get_taddress_transactions`
    #[allow(deprecated)]
    async fn get_taddress_txids(
        &self,
        request: TransparentAddressBlockFilter,
    ) -> Result<RawTransactionStream, Self::Error> {
        self.get_taddress_transactions(request).await
    }

    /// Returns the total balance for a list of taddrs
    async fn get_taddress_balance(&self, request: AddressList) -> Result<Balance, Self::Error> {
        let taddrs = GetAddressBalanceRequest::new(request.addresses);
        let balance = self.z_get_address_balance(taddrs).await?;
        let checked_balance: i64 = match i64::try_from(u64::from(balance.balance)) {
            Ok(balance) => balance,
            Err(_) => {
                return Err(NodeBackedIndexerServiceError::TonicStatusError(
                    tonic::Status::unknown("Error: Error converting balance from u64 to i64."),
                ));
            }
        };
        Ok(Balance {
            value_zat: checked_balance,
        })
    }

    /// Returns the total balance for a list of taddrs
    #[allow(deprecated)]
    async fn get_taddress_balance_stream(
        &self,
        mut request: AddressStream,
    ) -> Result<Balance, Self::Error> {
        let status = NodeBackedIndexerServiceError::TonicStatusError;
        let total_balance = timeout(
            time::Duration::from_secs(u64::from(self.config.service.timeout) * 4),
            async {
                // each address once, at most the limit, then one query over
                // one snapshot (Q-45, as lightwalletd)
                let mut seen = std::collections::HashSet::new();
                let mut addresses = Vec::new();
                while let Some(address) = request.next().await {
                    let address = address
                        .map_err(|e| {
                            tonic::Status::unknown(format!("Failed to read from stream: {e}"))
                        })?
                        .address;
                    if !seen.insert(address.clone()) {
                        continue;
                    }
                    super::validate_balance_address_count(seen.len())?;
                    addresses.push(address);
                }
                let snapshot = self.indexer.snapshot_nonfinalized_state();
                let balance = self
                    .indexer
                    .address_balance_in(&snapshot, GetAddressBalanceRequest::new(addresses))
                    .await
                    .map_err(|error| {
                        tonic::Status::from(NodeBackedIndexerServiceError::from(error))
                    })?;
                Ok(u64::from(balance.balance))
            },
        )
        .await
        .unwrap_or_else(|_| {
            Err(tonic::Status::deadline_exceeded(
                "Error: get_taddress_balance_stream request timed out.",
            ))
        })
        .map_err(status)?;

        Ok(Balance {
            value_zat: i64::try_from(total_balance).map_err(|_| {
                status(tonic::Status::unknown(
                    "Error: Error converting balance from u64 to i64.",
                ))
            })?,
        })
    }

    /// Returns a stream of the compact transaction representation for transactions
    /// currently in the mempool. The results of this operation may be a few
    /// seconds out of date. If the `exclude_txid_suffixes` list is empty,
    /// return all transactions; otherwise return all *except* those in the
    /// `exclude_txid_suffixes` list (if any); this allows the client to avoid
    /// receiving transactions that it already has (from an earlier call to this
    /// RPC). The transaction IDs in the `exclude_txid_suffixes` list can be
    /// shortened to any number of bytes to make the request more
    /// bandwidth-efficient; if two or more transactions in the mempool match a
    /// txid suffix, none of the matching transactions are excluded. Txid
    /// suffixes in the exclude list that don't match any transactions in the
    /// mempool are ignored.
    #[allow(deprecated)]
    async fn get_mempool_tx(
        &self,
        request: GetMempoolTxRequest,
    ) -> Result<CompactTransactionStream, Self::Error> {
        // Passed through as raw bytes. This used to reverse each suffix and hex
        // it so the mempool could match on strings; the mempool now matches on
        // the bytes directly, against a txid list sorted in the same reversed
        // order, so the round trip through hex was pure loss — and the length
        // and count checks it hand-rolled now live with the matcher that
        // enforces them.
        let exclude_txids = request.exclude_txid_suffixes.clone();
        // as lightwalletd: no pools is the shielded ones, an invalid pool is
        // refused, and a transaction with nothing in them is dropped (#44)
        let pools = PoolTypeFilter::new_from_slice(&request.pool_types)
            .map(|filter| zaino_proto::compact::pool_filter_from_wire(&filter))
            .map_err(|error| {
                NodeBackedIndexerServiceError::from(
                    zaino_proto::proto::utils::GetBlockRangeError::PoolTypeArgumentError(error),
                )
            })?;

        let mempool = self.indexer.clone();
        Ok(CompactTransactionStream::new(super::spawn_timed_stream(
            self.timeout_channel_size(),
            4,
            tonic::Status::internal("Error: get_mempool_tx gRPC request timed out"),
            |channel_tx| async move {
                match mempool.get_mempool_transactions(exclude_txids).await {
                    Ok(entries) => {
                        for entry in entries {
                            // the entry's compact form, decoded once at ingest
                            let compact = entry.compact.clone().filtered(pools);
                            if !compact.has_pool_data() {
                                continue;
                            }
                            // A mempool transaction is in no block, so index 0.
                            let compact = zaino_proto::compact::compact_tx_to_wire(0, &compact);

                            if channel_tx.send(Ok(compact)).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        // The ChainIndex error already carries its own gRPC
                        // status — `invalid_argument` for a malformed
                        // exclude list, `unavailable` for a retryable one.
                        // Flattening every one to `unknown` told the client
                        // nothing and made a caller mistake look like a
                        // server fault.
                        channel_tx
                            .send(Err(NodeBackedIndexerServiceError::from(e).into()))
                            .await
                            .ok();
                    }
                }
            },
        )))
    }

    /// Return a stream of current Mempool transactions. This will keep the output stream open while
    /// there are mempool transactions. It will close the returned stream when a new block is mined.
    #[allow(deprecated)]
    async fn get_mempool_stream(&self) -> Result<RawTransactionStream, Self::Error> {
        let indexer = self.indexer.clone();
        let snapshot = indexer.snapshot_nonfinalized_state();
        Ok(RawTransactionStream::new(super::spawn_timed_stream(
            self.timeout_channel_size(),
            6,
            tonic::Status::internal("Error: get_mempool_stream gRPC request timed out"),
            |channel_tx| async move {
                // The snapshot is passed in, not dropped: the stream must be
                // coherent with the tip this request was admitted against,
                // and `None` would take whatever the mempool is coherent
                // with instead.
                match indexer.get_mempool_stream(Some(&snapshot)) {
                    Some(mempool_stream) => {
                        let mut mempool_stream = std::pin::pin!(mempool_stream);
                        while let Some(result) = mempool_stream.next().await {
                            match result {
                                Ok(transaction_bytes) => {
                                    if channel_tx
                                        .send(Ok(RawTransaction {
                                            data: transaction_bytes,
                                            // A mempool transaction is
                                            // unmined, and `0` is the wire
                                            // sentinel for that. Reporting
                                            // the chain tip claimed it was
                                            // mined at a height it is not in.
                                            height: 0,
                                        }))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    channel_tx
                                        .send(Err(NodeBackedIndexerServiceError::from(e).into()))
                                        .await
                                        .ok();
                                    break;
                                }
                            }
                        }
                    }
                    None => {
                        // The caller's snapshot is older than the one the
                        // mempool is coherent with. Retryable, and
                        // `failed_precondition` says so — `internal` read as
                        // a server fault and gave the client nothing to act
                        // on.
                        warn!("mempool stream requested against a stale snapshot");
                        channel_tx
                            .send(Err(tonic::Status::failed_precondition(
                                "mempool is not coherent with the requested snapshot; \
                                     retry with a fresh snapshot",
                            )))
                            .await
                            .ok();
                    }
                };
            },
        )))
    }

    /// GetTreeState returns the note commitment tree state corresponding to the given block.
    /// See section 3.7 of the Zcash protocol specification. It returns several other useful
    /// values also (even though they can be obtained using GetBlock).
    /// The block can be specified by either height or hash.
    async fn get_tree_state(&self, request: BlockId) -> Result<TreeState, Self::Error> {
        let hash_or_height = blockid_to_hashorheight(request).ok_or(
            crate::error::NodeBackedIndexerServiceError::TonicStatusError(
                tonic::Status::invalid_argument("Invalid hash or height"),
            ),
        )?;

        let treestate_response =
            <Self as ZcashIndexer>::z_get_treestate(self, hash_or_height.to_string()).await?;

        Ok(super::tree_state_from_treestate_response(
            self.data.network().bip70_network_name(),
            treestate_response,
        ))
    }

    /// GetLatestTreeState returns the note commitment tree state corresponding to the chain tip.
    async fn get_latest_tree_state(&self) -> Result<TreeState, Self::Error> {
        // By hash, so the treestate is the tip's even if the chain moves.
        let tip = self.indexer.snapshot_nonfinalized_state().best_tip();
        let treestate = self.z_get_treestate(tip.hash.to_string()).await?;
        Ok(super::tree_state_from_treestate_response(
            self.data.network().bip70_network_name(),
            treestate,
        ))
    }

    #[allow(deprecated)]
    fn timeout_channel_size(&self) -> (u32, u32) {
        (
            self.config.service.timeout,
            self.config.service.channel_size,
        )
    }

    /// The subtree roots and their completing blocks, all from one snapshot
    /// (#49): each completing block's hash is the one the snapshot holds.
    async fn get_subtree_roots(
        &self,
        request: zaino_proto::proto::service::GetSubtreeRootsArg,
    ) -> Result<crate::stream::SubtreeRootReplyStream, Self::Error> {
        let (pool, start_index, limit) = super::subtree_roots_request(&request)
            .map_err(NodeBackedIndexerServiceError::TonicStatusError)?;
        let snapshot = self.indexer.snapshot_nonfinalized_state();
        let subtrees =
            zaino_chain::TreestateRead::subtree_roots(&snapshot, pool, start_index, limit)
                .await
                .map_err(crate::error::ChainIndexError::from)?;
        Ok(crate::stream::SubtreeRootReplyStream::new(
            super::spawn_timed_stream(
                self.timeout_channel_size(),
                4,
                tonic::Status::deadline_exceeded("Error: get_subtree_roots gRPC request timed out"),
                |channel_tx| async move {
                    for subtree in subtrees {
                        let height = subtree.end_height;
                        let reply =
                            match zaino_chain::BlockRead::block_hash(&snapshot, height).await {
                                Ok(Some(hash)) => {
                                    let mut display = <[u8; 32]>::from(hash);
                                    display.reverse();
                                    Ok(zaino_proto::proto::service::SubtreeRoot {
                                        root_hash: <[u8; 32]>::from(subtree.root).to_vec(),
                                        completing_block_hash: display.to_vec(),
                                        completing_block_height: u64::from(u32::from(height)),
                                    })
                                }
                                Ok(None) => Err(tonic::Status::internal(format!(
                                    "Error: no block at {height}, where a subtree completed."
                                ))),
                                Err(error) => {
                                    Err(tonic::Status::from(NodeBackedIndexerServiceError::from(
                                        crate::error::ChainIndexError::from(error),
                                    )))
                                }
                            };
                        let failed = reply.is_err();
                        if channel_tx.send(reply).await.is_err() || failed {
                            return;
                        }
                    }
                },
            ),
        ))
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted if
    /// [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are collected and returned as a single Vec.
    async fn get_address_utxos(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<GetAddressUtxosReplyList, Self::Error> {
        let address_utxos = self
            .requested_utxos(request)
            .await?
            .into_iter()
            .map(utxo_reply)
            .collect::<Result<Vec<_>, _>>()
            .map_err(NodeBackedIndexerServiceError::TonicStatusError)?;
        Ok(GetAddressUtxosReplyList { address_utxos })
    }

    /// Returns all unspent outputs for a list of addresses.
    ///
    /// Ignores all utxos below block height [GetAddressUtxosArg.start_height].
    /// Returns max [GetAddressUtxosArg.max_entries] utxos, or unrestricted
    /// if [GetAddressUtxosArg.max_entries] = 0.
    /// Utxos are returned in a stream.
    #[allow(deprecated)]
    async fn get_address_utxos_stream(
        &self,
        request: GetAddressUtxosArg,
    ) -> Result<UtxoReplyStream, Self::Error> {
        let utxos = self.requested_utxos(request).await?;
        Ok(UtxoReplyStream::new(super::spawn_timed_stream(
            self.timeout_channel_size(),
            4,
            tonic::Status::deadline_exceeded(
                "Error: get_address_utxos_stream gRPC request timed out",
            ),
            |channel_tx| async move {
                for reply in utxos.into_iter().map(utxo_reply) {
                    let failed = reply.is_err();
                    if channel_tx.send(reply).await.is_err() || failed {
                        return;
                    }
                }
            },
        )))
    }

    /// Return information about this lightwalletd instance and the blockchain
    async fn get_lightd_info(&self) -> Result<LightdInfo, Self::Error> {
        let blockchain_info = self.get_blockchain_info().await?;
        // proto3's zero and empty string are "not scheduled" (K-02)
        let schedule =
            zaino_service::schedule_at(&blockchain_info.upgrades, blockchain_info.blocks);
        let consensus_branch_id = zebra_chain::parameters::ConsensusBranchId::from(u32::from(
            blockchain_info.consensus.chain_tip,
        ))
        .to_string();

        Ok(LightdInfo {
            version: self.data.build_info().version(),
            vendor: "ZingoLabs ZainoD".to_string(),
            taddr_support: true,
            chain_name: blockchain_info.chain.clone(),
            sapling_activation_height: schedule
                .sapling_activation
                .map_or(0, |height| u64::from(u32::from(height))),
            consensus_branch_id,
            block_height: u64::from(u32::from(blockchain_info.blocks)),
            git_commit: self.data.build_info().commit_hash(),
            branch: self.data.build_info().branch(),
            build_date: self.data.build_info().build_date(),
            build_user: self.data.build_info().build_user(),
            estimated_height: u64::from(u32::from(blockchain_info.estimated_height)),
            zcashd_build: self.data.zebra_build(),
            zcashd_subversion: self.data.zebra_subversion(),
            donation_address: self
                .config
                .donation_address
                .as_ref()
                .map(DonationAddress::encode)
                .unwrap_or_default(),
            upgrade_name: schedule
                .next_upgrade
                .map(|upgrade| upgrade.name.clone())
                .unwrap_or_default(),
            upgrade_height: schedule
                .next_upgrade
                .map_or(0, |upgrade| u64::from(u32::from(upgrade.activation_height))),
            lightwallet_protocol_version: "v0.6.0".to_string(),
        })
    }

    async fn get_standard_fee(
        &self,
    ) -> Result<zaino_proto::proto::service::StandardFee, Self::Error> {
        // as lightwalletd (#59): unreachable is `UNAVAILABLE`, and a zero fee,
        // which no sane validator reports, is refused
        let status = NodeBackedIndexerServiceError::TonicStatusError;
        let fee = ZcashIndexer::get_standard_fee(self)
            .await
            .map_err(|error| match unreachable(&error) {
                true => status(tonic::Status::unavailable(error.to_string())),
                false => error,
            })?;
        nonzero_standard_fee(&fee).map_err(status)
    }

    /// Testing-only in lightwalletd, behind `--ping-very-insecure`; Zaino
    /// never enables it, and answers as lightwalletd does when disabled.
    async fn ping(&self, _request: Duration) -> Result<PingResponse, Self::Error> {
        Err(NodeBackedIndexerServiceError::TonicStatusError(
            tonic::Status::failed_precondition("Error: Ping is not enabled."),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{nonzero_standard_fee, StandardFee};
    use zaino_primitives::types::Zatoshis;

    /// A zero standard fee is refused; any other is served.
    #[test]
    fn a_zero_standard_fee_is_refused() {
        let fee = |zatoshis| StandardFee {
            fee_per_action: Zatoshis::new(zatoshis).expect("in range"),
            version: 0,
        };
        let refused = nonzero_standard_fee(&fee(0)).expect_err("zero");
        assert_eq!(refused.code(), tonic::Code::Internal);
        assert_eq!(
            nonzero_standard_fee(&fee(5_000)).expect("served").fee_zat,
            5_000
        );
    }
}
