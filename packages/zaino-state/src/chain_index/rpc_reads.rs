//! `ChainIndexRpcExt`: compact blocks and the txout set from the chain view,
//! node-only RPCs from the validator.

use futures::StreamExt as _;
use zaino_chain::{BlockId, BlockRead as _, CompactBlockRead as _};
use zaino_consensus::validate_raw_transaction_hex;
use zaino_primitives::types::rpc::{
    AddressDeltas, AddressDeltasRequest, BlockDeltas, BlockHeaderVerbose, BlockSubsidy, MiningInfo,
    NodeInfo, PeerInfo, StandardFee,
};
use zaino_primitives::types::PoolFilter;
use zaino_primitives::types::{MempoolInfo, TxOutSetInfo};
use zaino_proto::compact::{compact_block_to_wire, pool_filter_from_wire};
use zaino_proto::proto::compact_formats::CompactBlock;
use zaino_proto::proto::utils::PoolTypeFilter;
use zaino_source::{GetBlockDeltasError, GetBlockHeaderError, GetSpentInfoError, QueryError};
use zebra_rpc::methods::GetBlock;
use zebra_state::HashOrHeight;

use super::chain_head::WithChainHeadSource;
use super::chain_store::WithChainStoreSource;
use super::chain_view::{BestTip as _, ChainIndexSnapshot, WithChainViewSource};
use super::node_backed::NodeBackedChainIndexSubscriber;
use super::reads::flatten;
use super::source::{BlockchainSource, BlockchainSourceResult};
use super::types::{self, domain_height};
use super::validator_source::{err, parse_display_hash32, spent_info_err};
use super::{ChainIndex, ChainIndexRpcExt};
use crate::error::ChainIndexError;
use crate::CompactBlockStream;

/// A validator answer, with its failure in this crate's vocabulary.
fn validator<T>(result: BlockchainSourceResult<T>) -> Result<T, ChainIndexError> {
    result.map_err(ChainIndexError::backing_validator)
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > NodeBackedChainIndexSubscriber<Source>
{
    /// The compact block at `height` under `pools`; `None` past the tip.
    pub(crate) async fn compact_block(
        &self,
        snapshot: &ChainIndexSnapshot<Source>,
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
        snapshot: &ChainIndexSnapshot<Source>,
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

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > ChainIndexRpcExt for NodeBackedChainIndexSubscriber<Source>
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
        // zebra's `getblock` does (`-1` is the tip). A rejected identifier
        // carries the legacy `InvalidParameter` code for the serving layer.
        let snapshot = self.snapshot_nonfinalized_state();
        let tip = snapshot.best_tip().height;
        let id = HashOrHeight::new(
            &hash_or_height,
            Some(zebra_chain::block::Height(u32::from(tip))),
        )
        .map_err(|error| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                error,
            ))
        })?;
        // a height this snapshot holds is asked by its hash (D6-20); one past
        // it gets the validator's own answer
        let id = match id {
            HashOrHeight::Height(height) => match domain_height(types::Height(height.0)) {
                Some(at) => match snapshot.block_hash(at).await? {
                    Some(hash) => HashOrHeight::Hash(zebra_chain::block::Hash(hash.into())),
                    None => id,
                },
                None => id,
            },
            HashOrHeight::Hash(_) => id,
        };
        validator(self.source().get_block_verbose(id, verbosity).await)
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
        validator(self.source().get_difficulty().await)
    }

    async fn get_info(&self) -> Result<NodeInfo, Self::Error> {
        validator(self.source().get_info().await)
    }

    async fn get_blockchain_info(
        &self,
    ) -> Result<zaino_primitives::types::BlockchainInfo, Self::Error> {
        validator(self.source().get_blockchain_info().await)
    }

    async fn get_peer_info(&self) -> Result<Vec<PeerInfo>, Self::Error> {
        validator(self.source().get_peer_info().await)
    }

    async fn get_block_subsidy(&self, height: u32) -> Result<BlockSubsidy, Self::Error> {
        validator(self.source().get_block_subsidy(height).await)
    }

    async fn get_standard_fee(&self) -> Result<StandardFee, Self::Error> {
        validator(self.source().get_standard_fee().await)
    }

    async fn get_mining_info(&self) -> Result<MiningInfo, Self::Error> {
        validator(self.source().get_mining_info().await)
    }

    async fn get_tx_out(
        &self,
        txid: String,
        n: u32,
        include_mempool: Option<bool>,
    ) -> Result<Option<zaino_primitives::types::rpc::TxOut>, Self::Error> {
        validator(self.source().get_tx_out(txid, n, include_mempool).await)
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
        validator(self.source().get_network_sol_ps(blocks, height).await)
    }

    async fn send_raw_transaction(
        &self,
        raw_transaction_hex: String,
    ) -> Result<zaino_primitives::types::TransactionId, Self::Error> {
        // Rejected locally before the validator round trip, with the legacy
        // `InvalidParameter` code the validator would have given.
        validate_raw_transaction_hex(&raw_transaction_hex).map_err(|error| {
            ChainIndexError::internal_from(crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                error.to_string(),
            ))
        })?;
        validator(
            self.source()
                .send_raw_transaction(raw_transaction_hex)
                .await,
        )
    }

    async fn get_treestate_by_id(
        &self,
        hash_or_height: String,
    ) -> Result<zaino_primitives::types::Treestate, Self::Error> {
        validator(self.source().get_treestate_by_id(hash_or_height).await)
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

    async fn get_tx_out_set_info(&self) -> Result<Option<TxOutSetInfo>, Self::Error> {
        let snapshot = self.snapshot_nonfinalized_state();
        match self.reads(&snapshot).txout_set_info().await {
            Ok(info) => Ok(Some(info)),
            Err(zaino_service::ServiceError::NotServed { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
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
