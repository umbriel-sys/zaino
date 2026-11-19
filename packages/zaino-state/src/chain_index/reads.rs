//! `ChainIndex`, answered by the chain view and the mempool.

use std::collections::HashSet;

use futures::{Stream, StreamExt as _};
use zaino_chain::{
    AddressRead as _, BlockId, BlockRead as _, ForkReconcile as _, Locator, SpendRead as _,
    TreestateRead as _,
};
use zebra_rpc::client::{GetAddressBalanceRequest, GetAddressTxIdsRequest};

use super::chain_head::WithChainHeadSource;
use super::chain_store::WithChainStoreSource;
use super::chain_view::{BestTip as _, ChainIndexSnapshot, WithChainViewSource};
use super::node_backed::NodeBackedChainIndexSubscriber;
use super::source::{BlockchainSource, BlockchainSourceError, PoolTreestate};
use super::types::{
    self, block_index, chain_locations, checked_addresses, domain_hash, domain_height,
    domain_scope, domain_txid, local_hash, local_height, local_txid, BestChainLocation, BlockIndex,
    NonBestChainLocation,
};
use super::{ChainIndex, ShieldedPool};
use crate::error::{ChainIndexError, ChainIndexErrorKind};

/// A chain view's chunked range, one item at a time.
pub(super) fn flatten<T>(
    chunks: impl Stream<Item = zaino_chain::Result<Vec<T>>>,
) -> impl Stream<Item = Result<T, ChainIndexError>> {
    chunks.flat_map(|chunk| {
        futures::stream::iter(match chunk {
            Ok(items) => items.into_iter().map(Ok).collect::<Vec<_>>(),
            Err(error) => vec![Err(ChainIndexError::from(error))],
        })
    })
}

/// A request height, rejected as the legacy source rejected it.
fn request_height(height: u32) -> Result<zaino_primitives::types::Height, ChainIndexError> {
    zaino_primitives::types::Height::try_from(height).map_err(|error| {
        ChainIndexError::backing_validator(BlockchainSourceError::Unrecoverable(error.to_string()))
    })
}

impl<
        Source: BlockchainSource + WithChainHeadSource + WithChainStoreSource + WithChainViewSource,
    > ChainIndex for NodeBackedChainIndexSubscriber<Source>
{
    type Snapshot = ChainIndexSnapshot<Source>;
    type Error = ChainIndexError;

    fn snapshot_nonfinalized_state(&self) -> Self::Snapshot {
        self.service.chain_snapshot()
    }

    async fn get_block_height(
        &self,
        snapshot: &Self::Snapshot,
        hash: types::BlockHash,
    ) -> Result<Option<types::Height>, Self::Error> {
        Ok(snapshot
            .block_height(domain_hash(hash))
            .await?
            .map(local_height))
    }

    async fn get_block_hash(
        &self,
        snapshot: &Self::Snapshot,
        height: types::Height,
    ) -> Result<Option<types::BlockHash>, Self::Error> {
        let Some(height) = domain_height(height) else {
            return Ok(None);
        };
        Ok(snapshot.block_hash(height).await?.map(local_hash))
    }

    fn get_block_range(
        &self,
        snapshot: &Self::Snapshot,
        start: types::Height,
        end: Option<types::Height>,
    ) -> Option<impl Stream<Item = Result<Vec<u8>, Self::Error>>> {
        let tip = snapshot.best_tip().height;
        let at_or_below_tip = |height: types::Height| {
            domain_height(height)
                .filter(|height| *height <= tip)
                .ok_or_else(|| {
                    ChainIndexError::invalid_argument(format!(
                        "height {height} is above the chain tip {tip}"
                    ))
                })
        };
        let range = at_or_below_tip(start)
            .and_then(|start| Ok((start, end.map_or(Ok(tip), at_or_below_tip)?)));
        Some(match range {
            Ok((start, end)) => {
                futures::future::Either::Left(flatten(snapshot.stream_raw_blocks(start, end)))
            }
            Err(error) => {
                futures::future::Either::Right(futures::stream::once(async { Err(error) }))
            }
        })
    }

    async fn get_raw_transaction(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<Option<(Vec<u8>, Option<u32>)>, Self::Error> {
        // a mempool transaction's branch id is the one at the height it would
        // be mined at
        Ok(self
            .reads(snapshot)
            .transaction(domain_txid(txid))
            .await?
            .map(|served| {
                let branch_id = types::branch_id(&self.network, served.height);
                (served.bytes, branch_id)
            }))
    }

    async fn get_transaction_status(
        &self,
        snapshot: &Self::Snapshot,
        txid: &types::TransactionHash,
    ) -> Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), ChainIndexError> {
        let status = match self
            .reads(snapshot)
            .transaction_status(domain_txid(txid))
            .await
        {
            Ok(status) => status,
            Err(error @ zaino_service::ServiceError::MinedAndInMempool(_)) => {
                return Err(ChainIndexError::with_source(
                    ChainIndexErrorKind::InvalidSnapshot,
                    error,
                ))
            }
            Err(error) => return Err(error.into()),
        };
        chain_locations(status)
    }

    async fn get_mempool_txids(&self) -> Result<Vec<types::TransactionHash>, Self::Error> {
        Ok(self
            .service
            .mempool_txids()
            .iter()
            .copied()
            .map(local_txid)
            .collect())
    }

    async fn get_mempool_transactions(
        &self,
        exclude_list: Vec<Vec<u8>>,
    ) -> Result<Vec<std::sync::Arc<zaino_mempool::MempoolEntry>>, Self::Error> {
        Ok(self.service.mempool_entries(&exclude_list)?)
    }

    fn get_mempool_stream(
        &self,
        snapshot: Option<&Self::Snapshot>,
    ) -> Option<impl Stream<Item = Result<bytes::Bytes, Self::Error>>> {
        let stream = self.service.mempool_stream(snapshot)?;
        Some(stream.map(|item| {
            item.map_err(|e| {
                ChainIndexError::unavailable(format!("mempool stream ended early: {e}"))
            })
        }))
    }

    async fn best_chaintip(&self, snapshot: &Self::Snapshot) -> Result<BlockIndex, Self::Error> {
        Ok(block_index(snapshot.best_tip()))
    }

    async fn find_fork_point(
        &self,
        snapshot: &Self::Snapshot,
        hash: &types::BlockHash,
    ) -> Result<Option<(types::BlockHash, types::Height)>, Self::Error> {
        Ok(snapshot
            .fork_point(&Locator::new(vec![domain_hash(*hash)]))
            .await?
            .map(|fork| (local_hash(fork.hash), local_height(fork.height))))
    }

    async fn get_treestate(
        &self,
        hash: &types::BlockHash,
    ) -> Result<
        (
            Option<PoolTreestate>,
            Option<PoolTreestate>,
            Option<PoolTreestate>,
        ),
        Self::Error,
    > {
        let snapshot = self.service.chain_snapshot();
        if !Self::block_hash_known(&snapshot, *hash).await? {
            return Err(ChainIndexError::internal(format!(
                "block hash {hash} not found in local chain index"
            )));
        }
        let treestate = snapshot
            .treestate(BlockId::Hash(domain_hash(*hash)))
            .await?
            .ok_or_else(|| {
                ChainIndexError::internal(format!(
                    "failed to fetch treestate for block {hash} from validator"
                ))
            })?;
        Ok((treestate.sapling, treestate.orchard, treestate.ironwood))
    }

    async fn get_subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u16,
        max_entries: Option<u16>,
    ) -> Result<Vec<([u8; 32], u32)>, Self::Error> {
        Ok(self
            .service
            .chain_snapshot()
            .subtree_roots(ShieldedPool::to_domain(pool), start_index, max_entries)
            .await?
            .into_iter()
            .map(|root| (<[u8; 32]>::from(root.root), u32::from(root.end_height)))
            .collect())
    }

    async fn get_address_balance(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<zaino_primitives::types::AddressBalance, Self::Error> {
        let addresses = checked_addresses(&address_strings)?;
        Ok(self
            .service
            .chain_snapshot()
            .address_balance(&addresses)
            .await?)
    }

    async fn get_address_txids(
        &self,
        request: GetAddressTxIdsRequest,
    ) -> Result<Vec<types::TransactionHash>, Self::Error> {
        let addresses = checked_addresses(&request)?;
        let (_, start, end) = request.into_parts();
        Ok(self
            .service
            .chain_snapshot()
            .address_txids(&addresses, request_height(start)?, request_height(end)?)
            .await?
            .into_iter()
            .map(local_txid)
            .collect())
    }

    async fn get_address_utxos(
        &self,
        address_strings: GetAddressBalanceRequest,
    ) -> Result<Vec<zaino_primitives::types::Utxo>, Self::Error> {
        let addresses = checked_addresses(&address_strings)?;
        Ok(self
            .service
            .chain_snapshot()
            .address_utxos(&addresses, zaino_source::UtxoPage::default())
            .await?)
    }

    async fn get_outpoint_spenders(
        &self,
        snapshot: &Self::Snapshot,
        outpoints: Vec<types::Outpoint>,
        scope: types::ChainScope,
    ) -> Result<Vec<Option<types::TransactionHash>>, Self::Error> {
        let outpoints: Vec<_> = outpoints.iter().map(types::domain_outpoint).collect();
        snapshot
            .outpoint_spenders(&outpoints, domain_scope(scope))
            .await?
            .into_iter()
            .map(types::spender)
            .collect()
    }
}
