//! Output state: `gettxout`, `getspentinfo`, `sendrawtransaction`.

use zaino_primitives::types::{
    rpc::{ScriptPubKey, SpentInfo, SpentOutpoint, TxOut},
    BlockConfirmations, OutputIndex, ScriptType, TransactionId, TransparentAddressKey,
    TxConfirmations,
};
use zaino_source::{GetSpentInfoError, GetTxOutError, QueryError, SendRawTransactionError};

use super::ledger::{address_of, Ledger};
use crate::{CallArg, MockChain};

/// zcashd's `scriptPubKey` for a script: disassembly, type, signatures and
/// address for the standard forms.
pub(crate) fn script_pub_key(
    script: &zaino_primitives::types::Script,
    network: zaino_primitives::types::Network,
) -> ScriptPubKey {
    let bytes = script.as_bytes();
    let key = TransparentAddressKey::from_standard_script(bytes);
    let (asm, kind) = match key.map(|key| (key.script_type, hex::encode(key.hash))) {
        Some((ScriptType::P2PKH, hash)) => (
            Some(format!(
                "OP_DUP OP_HASH160 {hash} OP_EQUALVERIFY OP_CHECKSIG"
            )),
            "pubkeyhash",
        ),
        Some((ScriptType::P2SH, hash)) => {
            (Some(format!("OP_HASH160 {hash} OP_EQUAL")), "scripthash")
        }
        _ => (None, "nonstandard"),
    };
    let addresses = address_of(bytes, network)
        .and_then(|encoded| zaino_primitives::types::TransparentAddress::try_new(encoded).ok())
        .into_iter()
        .collect::<Vec<_>>();
    ScriptPubKey {
        script: script.clone(),
        asm,
        script_type: Some(kind.to_owned()),
        required_signatures: (!addresses.is_empty()).then_some(1),
        addresses,
    }
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
        let network = served.facts().network;
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
        if let Some(created) = ledger.outputs.get(&(txid, index)) {
            return Ok(Some(TxOut {
                best_block,
                confirmations: TxConfirmations::Mined(BlockConfirmations::of_best_chain_block(
                    created.height,
                    tip_height,
                )),
                value: created.output.value,
                script_pub_key: script_pub_key(&created.output.script, network),
                coinbase: created.coinbase,
            }));
        }
        if !include_mempool {
            return Ok(None);
        }
        let in_mempool = mempool
            .iter()
            .find(|(tx, _)| tx.txid == txid)
            .and_then(|(tx, _)| tx.transparent.outputs.get(usize::try_from(index).ok()?));
        Ok(in_mempool.map(|output| TxOut {
            best_block,
            confirmations: TxConfirmations::Mempool,
            value: output.value,
            script_pub_key: script_pub_key(&output.script, network),
            coinbase: false,
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
