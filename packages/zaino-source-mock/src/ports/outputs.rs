//! Output state: `gettxout`, `getspentinfo`, `sendrawtransaction`.

use zaino_primitives::types::{
    rpc::{ScriptPubKey, SpentInfo, SpentOutpoint, TxOut},
    BlockConfirmations, OutputIndex, TransactionId, TxConfirmations,
};
use zaino_source::{GetSpentInfoError, GetTxOutError, QueryError, SendRawTransactionError};

use super::fault;
use super::ledger::Ledger;
use crate::{CallArg, MockChain};

/// zebrad's `scriptPubKey` for a script: rendered by zebra-rpc's own
/// builder (the disassembly always, zebrad's script types) and read by the
/// zcash RPC adapter's parser.
fn script_pub_key(
    script: &zaino_primitives::types::Script,
    network: &zebra_chain::parameters::Network,
) -> Result<ScriptPubKey, QueryError<GetTxOutError>> {
    let output = zebra_chain::transparent::Output {
        value: zebra_chain::amount::Amount::zero(),
        lock_script: zebra_chain::transparent::Script::new(script.as_bytes()),
    };
    let object = zebra_rpc::client::ScriptPubKey::from_output(&output, network);
    let json =
        serde_json::to_value(&object).map_err(|error| fault(format!("scriptPubKey: {error}")))?;
    zaino_source_zcash_rpc::parse_script_pub_key_object(&json)
        .map_err(|error| fault(format!("scriptPubKey: {error}")))
}

/// A transaction's version from its consensus bytes: the header word less
/// the Overwinter flag.
fn version_of(bytes: Option<&Vec<u8>>) -> Result<u32, QueryError<GetTxOutError>> {
    bytes
        .and_then(|bytes| bytes.first_chunk::<4>())
        .map(|header| u32::from_le_bytes(*header) & 0x7fff_ffff)
        .ok_or_else(|| fault("a transaction built without its bytes".to_owned()))
}

impl zaino_source::OneShotGetTxOut for MockChain {
    /// The output if it exists and is unspent; `None` otherwise, as zcashd's
    /// `null`. With `include_mempool`, a mempool spend hides it and a
    /// mempool transaction's outputs are found (confirmations `Mempool`).
    async fn get_tx_out(
        &self,
        txid: TransactionId,
        index: OutputIndex,
        include_mempool: bool,
    ) -> Result<Option<TxOut>, QueryError<GetTxOutError>> {
        let served = self
            .enter(
                "get_tx_out",
                CallArg::TxOut {
                    txid,
                    index,
                    include_mempool,
                },
            )
            .await?;
        let Some(tip) = served.tip() else {
            return Ok(None);
        };
        let (best_block, tip_height) = (tip.hash(), tip.height());
        let network = super::tx_objects::zebra_network(&served)?;
        let ledger = Ledger::of(&served);
        let mempool = served.mempool();
        let spent_in_mempool = include_mempool
            && mempool.iter().any(|(tx, _)| {
                tx.transparent
                    .inputs
                    .iter()
                    .any(|input| input.prev_txid == txid && input.prev_index == index)
            });
        if ledger.spenders.contains_key(&(txid, index)) || spent_in_mempool {
            return Ok(None);
        }
        // genesis outputs were never in the UTXO set (consensus: never spendable)
        if let Some(created) = ledger
            .outputs
            .get(&(txid, index))
            .filter(|created| created.height != zaino_primitives::types::Height::GENESIS)
        {
            let bytes = served.at(created.height).and_then(|block| {
                block
                    .raw
                    .as_ref()?
                    .transactions
                    .get(usize::try_from(created.position).ok()?)
            });
            return Ok(Some(TxOut {
                best_block,
                confirmations: TxConfirmations::Mined(BlockConfirmations::of_best_chain_block(
                    created.height,
                    tip_height,
                )),
                value: created.output.value,
                script_pub_key: script_pub_key(&created.output.script, &network)?,
                coinbase: created.coinbase,
                version: version_of(bytes)?,
            }));
        }
        if !include_mempool {
            return Ok(None);
        }
        let Some((output, bytes)) =
            mempool
                .iter()
                .find(|(tx, _)| tx.txid == txid)
                .and_then(|(tx, bytes)| {
                    let output = tx.transparent.outputs.get(usize::try_from(index).ok()?)?;
                    Some((output, bytes))
                })
        else {
            return Ok(None);
        };
        Ok(Some(TxOut {
            best_block,
            confirmations: TxConfirmations::Mempool,
            value: output.value,
            script_pub_key: script_pub_key(&output.script, &network)?,
            coinbase: false,
            version: version_of(bytes.as_ref())?,
        }))
    }
}

impl zaino_source::OneShotGetSpentInfo for MockChain {
    /// The best-chain input that spent `outpoint`, or `NotSpent`.
    async fn get_spent_info(
        &self,
        outpoint: SpentOutpoint,
    ) -> Result<SpentInfo, QueryError<GetSpentInfoError>> {
        let served = self
            .enter("get_spent_info", CallArg::Outpoint(outpoint))
            .await?;
        let ledger = Ledger::of(&served);
        match ledger.spenders.get(&(outpoint.txid, outpoint.index)) {
            Some(spender) => Ok(SpentInfo {
                txid: spender.txid,
                index: spender.input,
                height: spender.height,
            }),
            None => Err(QueryError::Domain(GetSpentInfoError::NotSpent)),
        }
    }
}

impl zaino_source::OneShotSendRawTransaction for MockChain {
    /// Records the bytes and answers what [`MockChain::script_send`] set for
    /// them; unscripted bytes are `Rejected` (the mock validates nothing).
    async fn send_raw_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<TransactionId, QueryError<SendRawTransactionError>> {
        let mut served = self
            .enter("send_raw_transaction", CallArg::Bytes(transaction.clone()))
            .await?;
        let answer = served.scripted_send(&transaction);
        served.record_sent(transaction);
        match answer {
            Some(Ok(txid)) => Ok(txid),
            Some(Err(refusal)) => Err(QueryError::Domain(refusal)),
            None => Err(QueryError::Domain(SendRawTransactionError::Rejected(
                "mock: no answer scripted for these bytes".to_owned(),
            ))),
        }
    }
}
