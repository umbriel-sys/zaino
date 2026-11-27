//! `getblock` transaction objects, rendered by zebrad's own builders and read
//! by the zcash RPC adapter's parser, so the mock answers exactly as an
//! adapter reading zebrad does.

use std::collections::HashMap;
use std::sync::Arc;

use zaino_primitives::types::{rpc::TransactionVerbose, BlockVerbosity, Network};
use zaino_source::QueryError;
use zebra_chain::serialization::ZcashDeserializeInto as _;

use super::fault;
use super::ledger::Ledger;
use crate::chain::Served;
use crate::MockBlock;

/// The block's transaction objects at `verbosity` (2 or 3).
pub(crate) fn transaction_objects<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    block: &MockBlock,
    verbosity: BlockVerbosity,
    confirmations: i64,
) -> Result<Vec<TransactionVerbose>, QueryError<E>> {
    let bytes = block.raw.as_ref().ok_or_else(|| {
        fault(format!(
            "block {} was built without its bytes",
            block.height()
        ))
    })?;
    let zebra_block: zebra_chain::block::Block = bytes
        .block
        .as_slice()
        .zcash_deserialize_into()
        .map_err(|error| fault(format!("block bytes: {error}")))?;
    let network = zebra_network(served)?;
    let height = zebra_chain::block::Height(u32::from(block.height()));
    let hash = zebra_block.hash();
    let in_active_chain = served.is_best(block.hash());
    let spent = match verbosity {
        BlockVerbosity::TransactionsWithPrevouts => spent_utxos(served, &zebra_block)?,
        BlockVerbosity::TransactionIds | BlockVerbosity::Transactions => HashMap::new(),
    };
    zebra_block
        .transactions
        .iter()
        .map(|tx| {
            let mut object = zebra_rpc::client::TransactionObject::from_transaction(
                Arc::clone(tx),
                Some(height),
                Some(confirmations),
                &network,
                Some(zebra_block.header.time),
                Some(hash),
                Some(in_active_chain),
                tx.hash(),
            );
            if verbosity == BlockVerbosity::TransactionsWithPrevouts {
                object.add_prevouts(tx, &spent, &network);
            }
            read_back(&object)
        })
        .collect()
}

/// zebrad's object as the zcash RPC adapter reads it.
fn read_back<E: std::fmt::Debug + std::fmt::Display>(
    object: &zebra_rpc::client::TransactionObject,
) -> Result<TransactionVerbose, QueryError<E>> {
    let json = serde_json::to_value(object)
        .map_err(|error| fault(format!("transaction object: {error}")))?;
    zaino_source_zcash_rpc::parse_transaction_object(&json)
        .map_err(|error| fault(format!("transaction object: {error}")))
}

/// zebrad's verbose `getrawtransaction` for `txid`: placed by its best-chain
/// `block`, named by a side-chain one, or unplaced in the mempool.
pub(crate) fn transaction_object<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    txid: zaino_primitives::types::TransactionId,
    bytes: &[u8],
    block: Option<&MockBlock>,
) -> Result<TransactionVerbose, QueryError<E>> {
    read_back(&zebrad_object(served, txid, bytes, block)?)
}

/// The object zebrad's `getrawtransaction` builds for `txid`.
pub(crate) fn zebrad_object<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    txid: zaino_primitives::types::TransactionId,
    bytes: &[u8],
    block: Option<&MockBlock>,
) -> Result<zebra_rpc::client::TransactionObject, QueryError<E>> {
    let transaction: zebra_chain::transaction::Transaction = bytes
        .zcash_deserialize_into()
        .map_err(|error| fault(format!("transaction bytes: {error}")))?;
    let network = zebra_network(served)?;
    let hash = block.map(|block| zebra_chain::block::Hash(block.hash().into()));
    let best = block.filter(|block| served.is_best(block.hash()));
    let (height, confirmations, time) = match (best, served.tip()) {
        (Some(block), Some(tip)) => {
            let depth = u32::from(tip.height()).saturating_sub(u32::from(block.height()));
            let time = chrono::DateTime::from_timestamp(i64::from(block.block.header.time), 0)
                .ok_or_else(|| fault(format!("block {} has no valid time", block.height())))?;
            (
                Some(zebra_chain::block::Height(u32::from(block.height()))),
                Some(i64::from(depth).saturating_add(1)),
                Some(time),
            )
        }
        _ => (None, None, None),
    };
    Ok(zebra_rpc::client::TransactionObject::from_transaction(
        Arc::new(transaction),
        height,
        confirmations,
        &network,
        time,
        hash,
        Some(best.is_some()),
        zebra_chain::transaction::Hash(txid.into()),
    ))
}

/// The outputs `block` spends that the served best chain holds, as zebrad's
/// `SpentOutputs` answers them.
fn spent_utxos<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
    block: &zebra_chain::block::Block,
) -> Result<
    HashMap<zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo>,
    QueryError<E>,
> {
    let ledger = Ledger::of(served);
    let mut spent = HashMap::new();
    for outpoint in block
        .transactions
        .iter()
        .flat_map(|tx| tx.spent_outpoints())
    {
        let key = (
            zaino_primitives::types::TransactionId::from(outpoint.hash.0),
            outpoint.index,
        );
        let Some(created) = ledger.outputs.get(&key) else {
            continue;
        };
        let value = zebra_chain::amount::Amount::try_from(created.output.value.as_u64())
            .map_err(|error| fault(format!("prevout value: {error}")))?;
        let output = zebra_chain::transparent::Output {
            value,
            lock_script: zebra_chain::transparent::Script::new(created.output.script.as_bytes()),
        };
        spent.insert(
            outpoint,
            zebra_chain::transparent::Utxo::new(
                output,
                zebra_chain::block::Height(u32::from(created.height)),
                created.coinbase,
            ),
        );
    }
    Ok(spent)
}

/// zebra's network for the served chain's schedule.
pub(super) fn zebra_network<E: std::fmt::Debug + std::fmt::Display>(
    served: &Served<'_>,
) -> Result<zebra_chain::parameters::Network, QueryError<E>> {
    let facts = served.facts();
    Ok(match facts.network {
        Network::Mainnet => zebra_chain::parameters::Network::Mainnet,
        Network::PubTestnet => zebra_chain::parameters::Network::new_default_testnet(),
        Network::Regtest => crate::Activations::of_upgrades(&facts.upgrades)
            .activation_heights()
            .to_regtest_network(),
    })
}
