//! Transactions and the mempool.

use std::convert::Infallible;

use zaino_primitives::types::rpc::TransactionVerbosity;
use zaino_primitives::types::{BlockHash, Height, TransactionId};
use zaino_source::{
    GetMempoolMetadataError, GetMempoolTxidsError, GetRawMempoolTransactionError,
    GetTransactionError, MempoolTxMeta, QueryError, TransactionAnswer,
};

use super::{fault, refused};
use crate::{CallArg, MockChain};

impl zaino_source::OneShotGetTransaction for MockChain {
    /// `txid`'s bytes, or zebrad's object for where it is; a transaction the
    /// chain was built without bytes for is a fault, never empty bytes.
    async fn get_transaction(
        &self,
        txid: TransactionId,
        verbosity: TransactionVerbosity,
    ) -> Result<TransactionAnswer, QueryError<GetTransactionError>> {
        let served = self
            .enter("get_transaction", CallArg::Transaction { txid, verbosity })
            .await?;
        let Some((bytes, block)) = served.found(txid) else {
            return refused(GetTransactionError::NotFound(txid));
        };
        let bytes = bytes.ok_or_else(|| fault(format!("transaction {txid} has no bytes")))?;
        match verbosity {
            TransactionVerbosity::Raw => Ok(TransactionAnswer::Raw(bytes)),
            TransactionVerbosity::Object => {
                super::tx_objects::transaction_object(&served, txid, &bytes, block)
                    .map(|object| TransactionAnswer::Object(Box::new(object)))
            }
        }
    }
}

impl MockChain {
    /// The JSON zebrad's verbose `getrawtransaction` answers for `txid`,
    /// from zebra's own builder; `None` for a transaction it does not hold.
    /// Not a port call: nothing is logged.
    pub fn zebrad_transaction(&self, txid: TransactionId) -> Option<serde_json::Value> {
        let served = self.view();
        let (bytes, block) = served.found(txid)?;
        let object =
            super::tx_objects::zebrad_object::<GetTransactionError>(&served, txid, &bytes?, block)
                .ok()?;
        serde_json::to_value(&object).ok()
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
