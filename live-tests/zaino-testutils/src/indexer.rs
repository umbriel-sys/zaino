//! In-process `zainod` over a [`Validator`].

use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::TempDir;
use tonic::transport::Channel;
use zaino_proto::proto::compact_formats::{CompactBlock, CompactTx};
use zaino_proto::proto::service::compact_tx_streamer_client::CompactTxStreamerClient;
use zaino_proto::proto::service::{
    AddressList, Balance, BlockId, BlockRange, ChainSpec, Empty, GetAddressUtxosArg,
    GetAddressUtxosReply, GetMempoolTxRequest, GetSubtreeRootsArg, LightdInfo, RawTransaction,
    SendResponse, ShieldedProtocol, SubtreeRoot, TransparentAddressBlockFilter, TreeState,
    TxFilter,
};
use zaino_state::legacy::config::{DatabaseConfig, ServiceConfig, StorageConfig};
use zaino_state::{
    NodeBackedIndexerService, NodeBackedIndexerServiceConfig, NodeBackedIndexerServiceSubscriber,
};
use zainodlib::config::{BackendType, ValidatorConfig, ZainodConfig};
use zainodlib::indexer::Indexer;
use zcash_protocol::TxId;

use crate::process;
use crate::rpc::JsonRpcClient;
use crate::validator::Validator;

/// How the indexer ingests from the validator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// JSON-RPC.
    Fetch,
    /// zebra's on-disk state plus its indexer gRPC.
    State,
}

/// How to launch an indexer.
#[derive(Clone, Debug)]
pub struct IndexerConfig {
    /// Ingest path.
    pub backend: Backend,
    /// No persistent finalised database; finalised reads pass through.
    pub ephemeral: bool,
    network: zaino_primitives::types::Network,
    validator_rpc: SocketAddr,
    validator_grpc: SocketAddr,
    zebra_db_path: PathBuf,
    /// A finalised database to start from, copied into the run directory.
    pub db_from: Option<PathBuf>,
}

impl IndexerConfig {
    /// An indexer over `validator`.
    pub fn new(validator: &impl Validator, backend: Backend) -> Self {
        Self {
            backend,
            ephemeral: false,
            network: validator.network().zaino(),
            validator_rpc: validator.rpc_addr(),
            validator_grpc: validator.indexer_grpc_addr(),
            zebra_db_path: validator.state_dir().to_path_buf(),
            db_from: None,
        }
    }

    /// Finalised reads pass through to the validator.
    pub fn ephemeral(mut self) -> Self {
        self.ephemeral = true;
        self
    }

    /// Starts from a copy of the finalised database at `dir`.
    pub fn db_from(mut self, dir: impl Into<PathBuf>) -> Self {
        self.db_from = Some(dir.into());
        self
    }
}

/// A running in-process indexer. Dropping it aborts the indexer task.
pub struct Zainod {
    grpc_addr: SocketAddr,
    json_addr: SocketAddr,
    channel: Channel,
    subscriber: NodeBackedIndexerServiceSubscriber,
    task: tokio::task::JoinHandle<Result<(), zainodlib::error::IndexerError>>,
    _db_dir: TempDir,
}

impl std::fmt::Debug for Zainod {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Zainod")
            .field("grpc_addr", &self.grpc_addr)
            .field("json_addr", &self.json_addr)
            .finish_non_exhaustive()
    }
}

impl Zainod {
    /// Launches on OS-assigned loopback ports and waits until gRPC answers.
    /// The indexer's tracing follows `RUST_LOG`.
    pub async fn launch(config: IndexerConfig) -> Result<Self> {
        zaino_logging::try_init();
        let db_dir = tempfile::Builder::new().prefix("zaino-db-").tempdir()?;
        if let Some(src) = &config.db_from {
            process::copy_dir(src, &db_dir.path().join("db")).with_context(|| "copy zaino db")?;
        }
        let grpc_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let json_listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        let grpc_addr = grpc_listener.local_addr()?;
        let json_addr = json_listener.local_addr()?;

        let zainod_config = ZainodConfig {
            backend: match config.backend {
                Backend::Fetch => BackendType::Rpc,
                Backend::State => BackendType::Direct,
            },
            zebra_db_path: config.zebra_db_path,
            ephemeral_finalised_state: config.ephemeral,
            network: config.network,
            metrics_endpoint: None,
            json_server_settings: Some(zaino_serve::server::config::JsonRpcServerConfig {
                json_rpc_listen_address: json_addr,
                cookie_dir: None,
            }),
            grpc_settings: zaino_serve::server::config::GrpcServerConfig {
                listen_address: grpc_addr,
                tls: None,
            },
            validator_settings: ValidatorConfig {
                validator_grpc_listen_address: Some(config.validator_grpc.to_string()),
                validator_jsonrpc_listen_address: config.validator_rpc.to_string(),
                validator_cookie_path: None,
                validator_user: None,
                validator_password: None,
            },
            service: ServiceConfig::default(),
            storage: StorageConfig {
                database: DatabaseConfig {
                    path: db_dir.path().join("db"),
                    // A small map: the default 384 GB is a sparse file the
                    // cache would otherwise copy.
                    size: zaino_state::legacy::config::DatabaseSize(4),
                    ..Default::default()
                },
                ..Default::default()
            },
            mempool: Default::default(),
            donation_address: None,
        };
        let service_config = NodeBackedIndexerServiceConfig::try_from(zainod_config.clone())?;
        let (task, subscriber) = Indexer::<NodeBackedIndexerService>::launch_inner_with_listeners(
            service_config,
            zainod_config,
            grpc_listener,
            Some(json_listener),
        )
        .await?;

        let channel = Channel::from_shared(format!("http://{grpc_addr}"))?.connect_lazy();
        let zainod = Self {
            grpc_addr,
            json_addr,
            channel,
            subscriber,
            task,
            _db_dir: db_dir,
        };
        process::poll(
            "zainod gRPC answering",
            Duration::from_millis(100),
            Duration::from_secs(60),
            || async { zainod.indexer_info().await.ok() },
        )
        .await?;
        Ok(zainod)
    }

    /// gRPC listen address.
    pub fn grpc_addr(&self) -> SocketAddr {
        self.grpc_addr
    }

    /// `http://<grpc_addr>`, the URI wallets connect to.
    pub fn grpc_uri(&self) -> String {
        format!("http://{}", self.grpc_addr)
    }

    /// JSON-RPC listen address.
    pub fn json_addr(&self) -> SocketAddr {
        self.json_addr
    }

    /// The finalised database directory.
    pub fn db_dir(&self) -> PathBuf {
        self._db_dir.path().join("db")
    }

    /// A JSON-RPC client for the indexer.
    pub fn json_rpc(&self) -> JsonRpcClient {
        JsonRpcClient::new(self.json_addr)
    }

    /// The service subscriber: the in-process view a test can read directly.
    pub fn subscriber(&self) -> &NodeBackedIndexerServiceSubscriber {
        &self.subscriber
    }

    /// A gRPC client; cheap to clone and `Send`, for spawned streams.
    pub fn grpc(&self) -> CompactTxStreamerClient<Channel> {
        CompactTxStreamerClient::new(self.channel.clone())
    }

    /// Polls the served tip every 100 ms until it reaches `height`.
    pub async fn wait_for_block(&self, height: u32, timeout: Duration) -> Result<()> {
        process::poll(
            &format!("zainod serving height {height}"),
            Duration::from_millis(100),
            timeout,
            || async {
                let tip = self.latest_block_height().await.ok()?;
                (tip >= height).then_some(())
            },
        )
        .await
    }

    /// The finalised store's committed tip, `None` while empty or passed through.
    pub fn finalised_tip_height(&self) -> Option<u32> {
        self.subscriber.indexer.finalised_tip_height()
    }

    /// GetLatestBlock height.
    pub async fn latest_block_height(&self) -> Result<u32> {
        let block = self
            .grpc()
            .get_latest_block(ChainSpec {})
            .await?
            .into_inner();
        Ok(u32::try_from(block.height)?)
    }

    /// GetLightdInfo.
    pub async fn indexer_info(&self) -> Result<LightdInfo> {
        Ok(self.grpc().get_lightd_info(Empty {}).await?.into_inner())
    }

    /// GetBlock by height.
    pub async fn get_block(&self, height: u32) -> Result<CompactBlock> {
        Ok(self.grpc().get_block(block_id(height)).await?.into_inner())
    }

    /// GetBlock by hash (internal byte order).
    pub async fn get_block_by_hash(&self, hash: [u8; 32]) -> Result<CompactBlock> {
        Ok(self
            .grpc()
            .get_block(BlockId {
                height: 0,
                hash: hash.to_vec(),
            })
            .await?
            .into_inner())
    }

    /// GetBlockRange over `[start, end]` with the default (shielded) pools.
    pub async fn get_block_range(&self, start: u32, end: u32) -> Result<Vec<CompactBlock>> {
        self.get_block_range_with_pools(start, end, Vec::new())
            .await
    }

    /// GetBlockRange over `[start, end]` for `pool_types`.
    pub async fn get_block_range_with_pools(
        &self,
        start: u32,
        end: u32,
        pool_types: Vec<i32>,
    ) -> Result<Vec<CompactBlock>> {
        let mut stream = self
            .grpc()
            .get_block_range(block_range(start, end, pool_types))
            .await?
            .into_inner();
        let mut blocks = Vec::new();
        while let Some(block) = stream.message().await? {
            blocks.push(block);
        }
        Ok(blocks)
    }

    /// GetBlockRange, collecting what arrives before any error; the flag says
    /// whether the call or the stream errored.
    pub async fn drain_block_range(
        &self,
        start: u32,
        end: u32,
        pool_types: Vec<i32>,
    ) -> (Vec<CompactBlock>, bool) {
        let mut blocks = Vec::new();
        let Ok(response) = self
            .grpc()
            .get_block_range(block_range(start, end, pool_types))
            .await
        else {
            return (blocks, true);
        };
        let mut stream = response.into_inner();
        loop {
            match stream.message().await {
                Ok(Some(block)) => blocks.push(block),
                Ok(None) => return (blocks, false),
                Err(_) => return (blocks, true),
            }
        }
    }

    /// GetBlockNullifiers (deprecated on the wire; still served).
    #[allow(deprecated)]
    pub async fn get_block_nullifiers(&self, height: u32) -> Result<CompactBlock> {
        Ok(self
            .grpc()
            .get_block_nullifiers(block_id(height))
            .await?
            .into_inner())
    }

    /// GetBlockRangeNullifiers over `[start, end]` (deprecated on the wire; still served).
    #[allow(deprecated)]
    pub async fn get_block_range_nullifiers(
        &self,
        start: u32,
        end: u32,
    ) -> Result<Vec<CompactBlock>> {
        let mut stream = self
            .grpc()
            .get_block_range_nullifiers(block_range(start, end, Vec::new()))
            .await?
            .into_inner();
        let mut blocks = Vec::new();
        while let Some(block) = stream.message().await? {
            blocks.push(block);
        }
        Ok(blocks)
    }

    /// GetTreeState at `height`.
    pub async fn get_tree_state(&self, height: u32) -> Result<TreeState> {
        Ok(self
            .grpc()
            .get_tree_state(block_id(height))
            .await?
            .into_inner())
    }

    /// GetLatestTreeState.
    pub async fn get_latest_tree_state(&self) -> Result<TreeState> {
        Ok(self
            .grpc()
            .get_latest_tree_state(Empty {})
            .await?
            .into_inner())
    }

    /// GetSubtreeRoots.
    pub async fn get_subtree_roots(
        &self,
        start_index: u32,
        protocol: ShieldedProtocol,
        max_entries: u32,
    ) -> Result<Vec<SubtreeRoot>> {
        let mut stream = self
            .grpc()
            .get_subtree_roots(GetSubtreeRootsArg {
                start_index,
                shielded_protocol: protocol.into(),
                max_entries,
            })
            .await?
            .into_inner();
        let mut roots = Vec::new();
        while let Some(root) = stream.message().await? {
            roots.push(root);
        }
        Ok(roots)
    }

    /// GetTaddressTxids over `[start, end]`.
    pub async fn get_taddress_txids(
        &self,
        address: String,
        start: u32,
        end: u32,
    ) -> Result<Vec<RawTransaction>> {
        let mut stream = self
            .grpc()
            .get_taddress_txids(TransparentAddressBlockFilter {
                address,
                range: Some(block_range(start, end, Vec::new())),
            })
            .await?
            .into_inner();
        let mut txs = Vec::new();
        while let Some(tx) = stream.message().await? {
            txs.push(tx);
        }
        Ok(txs)
    }

    /// GetTaddressBalance.
    pub async fn get_taddress_balance(&self, addresses: Vec<String>) -> Result<Balance> {
        Ok(self
            .grpc()
            .get_taddress_balance(AddressList { addresses })
            .await?
            .into_inner())
    }

    /// GetAddressUtxos.
    pub async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
        start_height: u32,
        max_entries: u32,
    ) -> Result<Vec<GetAddressUtxosReply>> {
        Ok(self
            .grpc()
            .get_address_utxos(GetAddressUtxosArg {
                addresses,
                start_height: u64::from(start_height),
                max_entries,
            })
            .await?
            .into_inner()
            .address_utxos)
    }

    /// GetAddressUtxosStream.
    pub async fn get_address_utxos_stream(
        &self,
        addresses: Vec<String>,
        start_height: u32,
        max_entries: u32,
    ) -> Result<Vec<GetAddressUtxosReply>> {
        let mut stream = self
            .grpc()
            .get_address_utxos_stream(GetAddressUtxosArg {
                addresses,
                start_height: u64::from(start_height),
                max_entries,
            })
            .await?
            .into_inner();
        let mut utxos = Vec::new();
        while let Some(utxo) = stream.message().await? {
            utxos.push(utxo);
        }
        Ok(utxos)
    }

    /// GetMempoolTx, excluding `exclude` txid suffixes, default (shielded) pools.
    pub async fn get_mempool_tx(&self, exclude: Vec<Vec<u8>>) -> Result<Vec<CompactTx>> {
        let mut stream = self
            .grpc()
            .get_mempool_tx(GetMempoolTxRequest {
                exclude_txid_suffixes: exclude,
                pool_types: Vec::new(),
            })
            .await?
            .into_inner();
        let mut txs = Vec::new();
        while let Some(tx) = stream.message().await? {
            txs.push(tx);
        }
        Ok(txs)
    }

    /// GetMempoolStream, drained until the server closes it (on a new block).
    pub async fn get_mempool_stream(&self) -> Result<Vec<RawTransaction>> {
        let mut stream = self.grpc().get_mempool_stream(Empty {}).await?.into_inner();
        let mut txs = Vec::new();
        while let Some(tx) = stream.message().await? {
            txs.push(tx);
        }
        Ok(txs)
    }

    /// GetTransaction by txid.
    pub async fn get_transaction(&self, txid: &TxId) -> Result<RawTransaction> {
        Ok(self
            .grpc()
            .get_transaction(TxFilter {
                block: None,
                index: 0,
                hash: txid.as_ref().to_vec(),
            })
            .await?
            .into_inner())
    }

    /// SendTransaction.
    pub async fn send_transaction(&self, data: Vec<u8>) -> Result<SendResponse> {
        Ok(self
            .grpc()
            .send_transaction(RawTransaction {
                data: data.into(),
                ..Default::default()
            })
            .await?
            .into_inner())
    }

    /// Aborts the indexer task.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

impl Drop for Zainod {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn block_id(height: u32) -> BlockId {
    BlockId {
        height: u64::from(height),
        hash: Vec::new(),
    }
}

fn block_range(start: u32, end: u32, pool_types: Vec<i32>) -> BlockRange {
    BlockRange {
        start: Some(block_id(start)),
        end: Some(block_id(end)),
        pool_types,
    }
}

/// The `{code, message}` of a JSON-RPC error reply, or `Err` if the call succeeded.
pub async fn rpc_error(
    rpc: &JsonRpcClient,
    method: &str,
    params: serde_json::Value,
) -> Result<serde_json::Value> {
    let error = rpc
        .call_value(method, params)
        .await
        .err()
        .with_context(|| format!("{method} must be refused"))?;
    let text = error.to_string();
    let body = text
        .split_once("RPC error: ")
        .map(|(_, body)| body)
        .with_context(|| format!("{method}: no RPC error body in `{text}`"))?;
    let body: serde_json::Value = serde_json::from_str(body)?;
    Ok(serde_json::json!({ "code": body["code"], "message": body["message"] }))
}
