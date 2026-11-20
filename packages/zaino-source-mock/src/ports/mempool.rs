//! Transactions and the mempool.

use std::convert::Infallible;

use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zaino_source::{
    GetMempoolMetadataError, GetMempoolTxidsError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolTxMeta, QueryError, TransactionResponse,
};

use super::{fault, refused};
use crate::{CallArg, MockChain};

impl zaino_source::OneShotGetTransaction for MockChain {
    /// Where `txid` is, with its bytes; a transaction the chain was built
    /// without bytes for is a fault, never empty bytes.
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        let served = self.enter("get_transaction", CallArg::Txid(txid)).await?;
        let Some((location, mined)) = served.locate(txid) else {
            return refused(GetTransactionError::NotFound(txid));
        };
        let bytes = match mined {
            Some((block, position)) => block.raw.as_ref().and_then(|raw| {
                raw.transactions
                    .get(usize::try_from(position).ok()?)
                    .cloned()
            }),
            None => served
                .mempool()
                .into_iter()
                .find(|(tx, _)| tx.txid == txid)
                .and_then(|(_, bytes)| bytes),
        };
        Ok(TransactionResponse {
            bytes: bytes.ok_or_else(|| fault(format!("transaction {txid} has no bytes")))?,
            location,
        })
    }
}

impl zaino_source::OneShotGetMempoolTxids for MockChain {
    /// The mempool's txids in zebrad's order: ascending as displayed.
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>> {
        let served = self.enter("get_mempool_txids", CallArg::None).await?;
        let mut txids: Vec<TransactionId> = served
            .mempool()
            .into_iter()
            .map(|(tx, _)| tx.txid)
            .collect();
        txids.sort_by_key(|txid| {
            let mut displayed = <[u8; 32]>::from(*txid);
            displayed.reverse();
            displayed
        });
        Ok(txids)
    }
}

impl zaino_source::OneShotGetMempoolMetadata for MockChain {
    /// Every transaction entered at the mempool height, at that block's
    /// time (the mock's clock), as zebrad's verbose listing carries `time`.
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>> {
        let served = self.enter("get_mempool_metadata", CallArg::None).await?;
        let Some(tip) = served
            .mempool_height()
            .and_then(|height| served.at(Height::try_from(height).ok()?))
        else {
            return refused(GetMempoolMetadataError::Unavailable);
        };
        let (height, time) = (tip.height(), i64::from(tip.block.header.time));
        Ok(served
            .mempool()
            .into_iter()
            .map(|(tx, _)| MempoolTxMeta {
                txid: tx.txid,
                entry_height: height,
                entry_time: Some(time),
            })
            .collect())
    }
}

impl zaino_source::OneShotGetRawMempoolTransaction for MockChain {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        let served = self
            .enter("get_raw_mempool_transaction", CallArg::Txid(txid))
            .await?;
        match served.mempool().into_iter().find(|(tx, _)| tx.txid == txid) {
            Some((_, Some(bytes))) => Ok(bytes),
            Some((_, None)) => Err(fault(format!("mempool transaction {txid} has no bytes"))),
            None => refused(GetRawMempoolTransactionError::NotFound(txid)),
        }
    }
}

impl zaino_source::OneShotGetMempoolSourceTip for MockChain {
    /// The tip the mempool ports serve at (held behind by
    /// [`MockChain::mine_ahead_of_mempool`]).
    async fn get_mempool_source_tip(&self) -> Result<(BlockHash, Height), QueryError<Infallible>> {
        let served = self.enter("get_mempool_source_tip", CallArg::None).await?;
        served
            .mempool_height()
            .and_then(|height| served.at(Height::try_from(height).ok()?))
            .map(|block| (block.hash(), block.height()))
            .ok_or_else(|| fault("no chain to serve the mempool from"))
    }
}
