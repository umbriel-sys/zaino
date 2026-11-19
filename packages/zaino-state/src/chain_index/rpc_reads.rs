//! `ChainIndexRpcExt`: every read through the service, in the legacy shapes.

use futures::StreamExt as _;
use zaino_chain::{BlockId, CompactBlockRead as _, TreestateRead as _};
use zaino_consensus::validate_raw_transaction_hex;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo, StandardFee,
};
use zaino_primitives::types::{BlockVerbosity, PoolFilter};
use zaino_primitives::types::{MempoolInfo, TxOutSetInfo};
use zaino_proto::compact::{compact_block_to_wire, pool_filter_from_wire};
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::utils::PoolTypeFilter;
use zaino_service::ServiceSource;
use zaino_source::{
    GetBlockDeltasError, GetBlockHeaderError, GetBlockVerboseError, GetSpentInfoError,
    GetTreestateByHashError, GetTreestateError, QueryError,
};
use zebra_rpc::methods::GetBlock;
use zebra_state::HashOrHeight;

use super::chain_view::{BestTip as _, ChainIndexSnapshot};
use super::getblock_wire::block_to_wire;
use super::node_backed::NodeBackedChainIndexSubscriber;
use super::reads::flatten;
use super::source::BlockchainSourceResult;
use super::source_ports::ChainIndexSourcePorts;
use super::types::{self, domain_height};
use super::validator_source::{err, parse_display_hash32, parse_display_txid, spent_info_err};
use super::{ChainIndex, ChainIndexRpcExt};
use crate::error::ChainIndexError;
use crate::CompactBlockStream;

/// A validator answer, with its failure in this crate's vocabulary.
fn validator<T>(result: BlockchainSourceResult<T>) -> Result<T, ChainIndexError> {
    result.map_err(ChainIndexError::backing_validator)
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedChainIndexSubscriber<V> {
    /// The compact block at `height` under `pools`; `None` past the tip.
    pub(crate) async fn compact_block(
        &self,
        snapshot: &ChainIndexSnapshot<V>,
        height: types::Height,
        pools: PoolFilter,
    ) -> Result<Option<CompactBlock>, ChainIndexError> {
        let Some(at) = domain_height(height).filter(|at| *at <= snapshot.best_tip().height) else {
            return Ok(None);
        };
        let block = snapshot
            .compact_block(at, pools)
            .await?
            .ok_or_else(|| ChainIndexError::database_hole(height, None))?;
        Ok(Some(compact_block_to_wire(&block, pools)))
    }

    /// The compact blocks from `start_height` to `end_height` under `pools`.
    pub(crate) fn compact_block_stream(
        &self,
        snapshot: &ChainIndexSnapshot<V>,
        start_height: types::Height,
        end_height: types::Height,
        pools: PoolFilter,
    ) -> Result<Option<CompactBlockStream>, ChainIndexError> {
        let tip = u32::from(snapshot.best_tip().height);
        let ascending = start_height <= end_height;
        if !ascending && start_height.0 > tip {
            return Ok(None);
        }
        // Ascending past the tip: serve up to the tip, then say what was cut.
        let past_tip = ascending && end_height.0 > tip;
        let end = if past_tip {
            types::Height(tip)
        } else {
            end_height
        };
        let blocks = match (
            ascending && start_height.0 > tip,
            domain_height(start_height),
            domain_height(end),
        ) {
            (false, Some(start), Some(end)) => {
                Some(flatten(snapshot.stream_compact(start, end, pools)))
            }
            _ => None,
        };

        let (sender, receiver) = tokio::sync::mpsc::channel(128);
        tokio::spawn(async move {
            if let Some(blocks) = blocks {
                let mut blocks = std::pin::pin!(blocks);
                while let Some(block) = blocks.next().await {
                    let item = block
                        .map(|block| compact_block_to_wire(&block, pools))
                        .map_err(|error| tonic::Status::internal(error.to_string()));
                    if sender.send(item).await.is_err() {
                        return;
                    }
                }
            }
            if past_tip
                && sender
                    .send(Err(tonic::Status::out_of_range(format!(
                        "Error: Height out of range [{}]. Height requested is greater than the best chain tip [{}].",
                        end_height.0, tip,
                    ))))
                    .await
                    .is_err()
            {
                // the client went away: nobody to tell
            }
        });
        Ok(Some(CompactBlockStream::new(receiver)))
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> ChainIndexRpcExt
    for NodeBackedChainIndexSubscriber<V>
{
    async fn get_compact_block(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlock>, Self::Error> {
        self.compact_block(snapshot, height, pool_filter_from_wire(&pool_types))
            .await
    }

    async fn get_compact_block_stream(
        &self,
        snapshot: &Self::Snapshot,
        start_height: types::Height,
        end_height: types::Height,
        pool_types: PoolTypeFilter,
    ) -> Result<Option<CompactBlockStream>, Self::Error> {
        self.compact_block_stream(
            snapshot,
            start_height,
            end_height,
            pool_filter_from_wire(&pool_types),
        )
    }

    async fn z_get_block(
        &self,
        hash_or_height: String,
        verbosity: Option<u8>,
    ) -> Result<GetBlock, Self::Error> {
        // Tip-relative negative heights resolve against the best chaintip, as
        // zebra's `getblock` does (`-1` is the tip).
        let snapshot = self.snapshot_nonfinalized_state();
        let tip = snapshot.best_tip().height;
        let id = HashOrHeight::new(
            &hash_or_height,
            Some(zebra_chain::block::Height(u32::from(tip))),
        )
        .map_err(invalid_parameter)?;
        let at = match id {
            HashOrHeight::Hash(hash) => BlockId::Hash(hash.0.into()),
            HashOrHeight::Height(height) => match domain_height(types::Height(height.0)) {
                Some(height) => BlockId::Height(height),
                None => return Err(invalid_parameter("block height out of range")),
            },
        };
        let reads = self.reads(&snapshot);
        let level = verbosity.unwrap_or(1);
        if level == 0 {
            return match reads.raw_block(at).await? {
                Some(bytes) => Ok(GetBlock::Raw(bytes.into())),
                None => Err(block_not_found(at)),
            };
        }
        let verbosity = BlockVerbosity::from_level(level)
            .ok_or_else(|| invalid_parameter("invalid verbosity value"))?;
        let block = reads
            .block_verbose(at, verbosity)
            .await?
            .ok_or_else(|| block_not_found(at))?;
        block_to_wire(&block).map_err(ChainIndexError::internal_from)
    }

    async fn get_block_header(&self, hash: String) -> Result<BlockHeaderVerbose, Self::Error> {
        let hash = display_hash(&hash)?;
        let snapshot = self.snapshot_nonfinalized_state();
        self.reads(&snapshot)
            .block_header(BlockId::Hash(hash))
            .await?
            .ok_or_else(|| rejected(GetBlockHeaderError::BlockNotFound(hash)))
    }

    async fn get_raw_block_header(&self, hash: String) -> Result<Vec<u8>, Self::Error> {
        let hash = display_hash(&hash)?;
        let snapshot = self.snapshot_nonfinalized_state();
        self.reads(&snapshot)
            .raw_block_header(BlockId::Hash(hash))
            .await?
            .ok_or_else(|| rejected(GetBlockHeaderError::BlockNotFound(hash)))
    }

    async fn get_block_deltas(&self, hash: String) -> Result<BlockDeltas, Self::Error> {
        let hash = display_hash(&hash)?;
        let snapshot = self.snapshot_nonfinalized_state();
        self.reads(&snapshot)
            .block_deltas(hash)
            .await?
            .ok_or_else(|| rejected(GetBlockDeltasError::BlockNotFound(hash)))
    }

    async fn get_difficulty(&self) -> Result<f64, Self::Error> {
        Ok(self.tip_reads().difficulty_at_tip().await?)
    }

    async fn get_info(&self) -> Result<NodeInfo, Self::Error> {
        Ok(self.tip_reads().node_info().await?)
    }

    async fn get_blockchain_info(
        &self,
    ) -> Result<zaino_primitives::types::BlockchainInfo, Self::Error> {
        Ok(self.tip_reads().blockchain_info().await?)
    }

    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, Self::Error> {
        Ok(self.tip_reads().peer_info().await?)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<BlockSubsidy, Self::Error> {
        let height = validator(super::validator_source::domain_height(height))?;
        Ok(self.tip_reads().block_subsidy(Some(height)).await?)
    }

    async fn get_standard_fee(&self) -> Result<StandardFee, Self::Error> {
        Ok(self.tip_reads().standard_fee().await?)
    }

    async fn get_mining_info(&self) -> Result<MiningInfo, Self::Error> {
        Ok(self.tip_reads().mining_info().await?)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, Self::Error> {
        let txid = validator(parse_display_txid(&txid))?;
        // `include_mempool` defaults to true, as the interface does.
        Ok(self
            .tip_reads()
            .tx_out(txid, n, include_mempool.unwrap_or(true))
            .await?)
    }

    async fn get_spent_info(
        &self,
        outpoint: zaino_primitives::types::rpc::SpentOutpoint,
    ) -> Result<zaino_primitives::types::rpc::SpentInfo, Self::Error> {
        let snapshot = self.snapshot_nonfinalized_state();
        let rejection = match self.reads(&snapshot).spent_info(outpoint).await {
            Ok(Some(info)) => return Ok(info),
            Ok(None) => GetSpentInfoError::NotSpent,
            Err(zaino_service::ServiceError::NotServed { .. }) => GetSpentInfoError::Unsupported,
            Err(error) => return Err(error.into()),
        };
        Err(ChainIndexError::backing_validator(spent_info_err(
            QueryError::<_, zaino_source::NonDomainError>::Domain(rejection),
        )))
    }

    async fn get_network_sol_ps(
        &self,
        blocks: Option<i32>,
        height: Option<i32>,
    ) -> Result<u64, Self::Error> {
        // Negative values are how this interface spells "use your default".
        let blocks = blocks.and_then(|b| u32::try_from(b).ok());
        let height = match height.and_then(|h| u32::try_from(h).ok()) {
            Some(h) => Some(validator(super::validator_source::domain_height(h))?),
            None => None,
        };
        Ok(self.tip_reads().network_sol_ps(blocks, height).await?)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<zaino_primitives::types::TransactionId, Self::Error> {
        // Rejected locally before the validator round trip, with zebrad's
        // `Deserialization` code (methods.rs:1519-1522).
        let deserialization = |error: String| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::Deserialization,
                error,
            ))
        };
        validate_raw_transaction_hex(&raw_transaction_hex)
            .map_err(|error| deserialization(error.to_string()))?;
        let bytes = hex::decode(&raw_transaction_hex)
            .map_err(|error| deserialization(error.to_string()))?;
        Ok(self.tip_reads().send_raw_transaction(bytes).await?)
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> Result<zaino_primitives::types::Treestate, Self::Error> {
        let at = match hash_or_height.parse::<u32>() {
            Ok(height) => {
                BlockId::Height(validator(super::validator_source::domain_height(height))?)
            }
            Err(_) => BlockId::Hash(display_hash(&hash_or_height)?),
        };
        let snapshot = self.snapshot_nonfinalized_state();
        snapshot.treestate(at).await?.ok_or_else(|| match at {
            BlockId::Hash(hash) => rejected(GetTreestateByHashError::BlockNotFound(hash)),
            BlockId::Height(height) => rejected(GetTreestateError::HeightNotFound(height)),
        })
    }

    async fn get_address_deltas(
        &self,
        params: AddressDeltasRequest,
    ) -> Result<AddressDeltas, Self::Error> {
        let addresses = match &params {
            AddressDeltasRequest::Address(address) => vec![String::from(address.clone())],
            AddressDeltasRequest::Filtered { addresses, .. } => {
                addresses.iter().cloned().map(String::from).collect()
            }
        };
        types::checked_addresses(&zebra_rpc::client::GetAddressBalanceRequest::new(addresses))?;
        let snapshot = self.snapshot_nonfinalized_state();
        Ok(self.reads(&snapshot).address_deltas(params).await?)
    }

    async fn get_mempool_info(&self) -> MempoolInfo {
        // The tip-agnostic set: this reports what is in the mempool, not where
        // the chain is, so it must not freeze.
        let info = self.service.mempool_info();
        MempoolInfo {
            size: info.size,
            bytes: info.bytes,
            usage: info.usage,
        }
    }

    async fn get_tx_out_set_info(&self) -> Result<TxOutSetInfo, Self::Error> {
        let snapshot = self.snapshot_nonfinalized_state();
        Ok(self.reads(&snapshot).txout_set_info().await?)
    }
}

impl<V: ChainIndexSourcePorts + ServiceSource> NodeBackedChainIndexSubscriber<V> {
    /// The service's reads at the current tip.
    fn tip_reads(&self) -> zaino_service::ServiceSnapshot<V> {
        self.reads(&self.snapshot_nonfinalized_state())
    }
}

/// A block hash in RPC display order.
fn display_hash(hash: &str) -> Result<zaino_primitives::types::BlockHash, ChainIndexError> {
    validator(parse_display_hash32(hash).map(zaino_primitives::types::BlockHash::from))
}

/// A refusal in the shape the validator's port reported it.
fn rejected(rejection: impl std::fmt::Debug + std::fmt::Display) -> ChainIndexError {
    ChainIndexError::backing_validator(err(QueryError::<_, zaino_source::NonDomainError>::Domain(
        rejection,
    )))
}

/// A malformed parameter, with the legacy `InvalidParameter` code.
fn invalid_parameter(error: impl std::fmt::Display) -> ChainIndexError {
    ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
        zebra_rpc::server::error::LegacyCode::InvalidParameter,
        error.to_string(),
    ))
}

/// `getblock` for a block the snapshot does not hold.
fn block_not_found(at: BlockId) -> ChainIndexError {
    rejected(match at {
        BlockId::Hash(hash) => GetBlockVerboseError::BlockNotFound(hash),
        BlockId::Height(height) => GetBlockVerboseError::HeightNotFound(height),
    })
}
