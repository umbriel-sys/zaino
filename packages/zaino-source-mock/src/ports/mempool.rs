//! Transactions and the mempool.

use std::convert::Infallible;

use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zaino_source::{
    GetMempoolMetadataError, GetMempoolTxidsError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolTxMeta, QueryError, TransactionResponse,
};

use super::{fault, refused};
use crate::MockChain;

impl zaino_source::OneShotGetTransaction for MockChain {
    /// Where `txid` is, with its bytes where the chain has them (else empty).
    async fn get_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<TransactionResponse, QueryError<GetTransactionError>> {
        let served = self.enter("get_transaction")?;
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
            bytes: bytes.unwrap_or_default(),
            location,
        })
    }
}

impl zaino_source::OneShotGetMempoolTxids for MockChain {
    async fn get_mempool_txids(
        &self,
    ) -> Result<Vec<TransactionId>, QueryError<GetMempoolTxidsError>> {
        let served = self.enter("get_mempool_txids")?;
        Ok(served
            .mempool()
            .into_iter()
            .map(|(tx, _)| tx.txid)
            .collect())
    }
}

impl zaino_source::OneShotGetMempoolMetadata for MockChain {
    /// Every transaction entered at the mempool height; no arrival time.
    async fn get_mempool_metadata(
        &self,
    ) -> Result<Vec<MempoolTxMeta>, QueryError<GetMempoolMetadataError>> {
        let served = self.enter("get_mempool_metadata")?;
        let Some(height) = served
            .mempool_height()
            .and_then(|height| Height::try_from(height).ok())
        else {
            return refused(GetMempoolMetadataError::Unavailable);
        };
        Ok(served
            .mempool()
            .into_iter()
            .map(|(tx, _)| MempoolTxMeta {
                txid: tx.txid,
                entry_height: height,
                entry_time: None,
            })
            .collect())
    }
}

impl zaino_source::OneShotGetRawMempoolTransaction for MockChain {
    async fn get_raw_mempool_transaction(
        &self,
        txid: TransactionId,
    ) -> Result<Vec<u8>, QueryError<GetRawMempoolTransactionError>> {
        let served = self.enter("get_raw_mempool_transaction")?;
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
        let served = self.enter("get_mempool_source_tip")?;
        served
            .mempool_height()
            .and_then(|height| served.at(Height::try_from(height).ok()?))
            .map(|block| (block.hash(), block.height()))
            .ok_or_else(|| fault("no chain to serve the mempool from"))
    }
}
