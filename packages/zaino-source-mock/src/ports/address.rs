//! The address index: balance, UTXOs, txids, deltas.
//!
//! As zcashd's insight index and the zebra adapter's synthesis of it answer:
//! over the confirmed best chain (no mempool); a malformed or TEX address is
//! the port's `InvalidAddress`; an answer over
//! [`zaino_source::MAX_ADDRESS_QUERY_RESULTS`] is `TooManyResults`.

use std::collections::BTreeSet;

use zaino_primitives::types::{
    AddressBalance, AddressDelta, Height, Network, SignedZatoshis, TransactionId,
    TransparentAddress, Utxo, Zatoshis, ZatoshisFlowSum,
};
use zaino_source::{
    GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError, GetAddressUtxosError,
    QueryError, MAX_ADDRESS_QUERY_RESULTS,
};

use super::fault;
use super::ledger::{address_of, Ledger};
use crate::chain::Served;
use crate::{CallArg, MockChain};

/// The requested addresses, validated as the port defines: transparent, and
/// for the node's network.
fn requested<E: std::fmt::Debug + std::fmt::Display>(
    addresses: Vec<String>,
    network: Network,
    invalid: impl Fn(String) -> E,
) -> Result<BTreeSet<String>, QueryError<E>> {
    addresses
        .into_iter()
        .map(
            |address| match TransparentAddress::try_new(address.clone()) {
                Ok(parsed) if parsed.is_for(network) => Ok(address),
                Ok(_) => Err(QueryError::Domain(invalid(format!(
                    "an address for another network: {address}"
                )))),
                Err(why) => Err(QueryError::Domain(invalid(format!(
                    "invalid transparent address `{address}`: {why}"
                )))),
            },
        )
        .collect()
}

fn addresses_arg(addresses: &[String], range: Option<(Height, Height)>) -> CallArg {
    CallArg::Addresses {
        addresses: addresses.to_vec(),
        range,
    }
}

fn capped<E>(rows: usize, too_many: impl FnOnce(usize) -> E) -> Result<(), QueryError<E>>
where
    E: std::fmt::Debug + std::fmt::Display,
{
    match rows > MAX_ADDRESS_QUERY_RESULTS {
        true => Err(QueryError::Domain(too_many(MAX_ADDRESS_QUERY_RESULTS))),
        false => Ok(()),
    }
}

fn address<E: std::fmt::Debug + std::fmt::Display>(
    encoded: String,
) -> Result<TransparentAddress, QueryError<E>> {
    TransparentAddress::try_new(encoded).map_err(|why| fault(why.to_string()))
}

impl zaino_source::OneShotGetAddressBalance for MockChain {
    /// `balance` = the unspent outputs paying the addresses; `received` = every
    /// output that ever paid them.
    async fn get_address_balance(
        &self,
        addresses: Vec<String>,
    ) -> Result<AddressBalance, QueryError<GetAddressBalanceError>> {
        let served = self
            .enter("get_address_balance", addresses_arg(&addresses, None))
            .await?;
        let network = served.facts().network;
        let wanted = requested(addresses, network, GetAddressBalanceError::InvalidAddress)?;
        let ledger = Ledger::of(&served);
        let mut received = Vec::new();
        let mut unspent = Vec::new();
        for (outpoint, created) in &ledger.outputs {
            let pays = address_of(created.output.script.as_bytes(), network)
                .is_some_and(|address| wanted.contains(&address));
            if !pays {
                continue;
            }
            received.push(created.output.value);
            if !ledger.spenders.contains_key(outpoint) {
                unspent.push(created.output.value);
            }
        }
        Ok(AddressBalance {
            balance: Zatoshis::sum_balances(unspent.into_iter())
                .ok_or_else(|| fault("unspent outputs past the supply"))?,
            received: ZatoshisFlowSum::try_accumulate(received.into_iter())
                .ok_or_else(|| fault("received flow overflowed"))?,
        })
    }
}

impl zaino_source::OneShotGetAddressUtxos for MockChain {
    /// Unspent outputs paying the addresses, by (height, position, index).
    async fn get_address_utxos(
        &self,
        addresses: Vec<String>,
    ) -> Result<Vec<Utxo>, QueryError<GetAddressUtxosError>> {
        let served = self
            .enter("get_address_utxos", addresses_arg(&addresses, None))
            .await?;
        let network = served.facts().network;
        let wanted = requested(addresses, network, GetAddressUtxosError::InvalidAddress)?;
        let ledger = Ledger::of(&served);
        let mut rows: Vec<_> = ledger
            .outputs
            .iter()
            .filter(|(outpoint, _)| !ledger.spenders.contains_key(outpoint))
            .filter_map(|(outpoint, created)| {
                let encoded = address_of(created.output.script.as_bytes(), network)?;
                wanted
                    .contains(&encoded)
                    .then_some((outpoint, created, encoded))
            })
            .collect();
        capped(rows.len(), |limit| GetAddressUtxosError::TooManyResults {
            limit,
        })?;
        rows.sort_by_key(|((_, index), created, _)| (created.height, created.position, *index));
        rows.into_iter()
            .map(|((txid, index), created, encoded)| {
                Ok(Utxo {
                    address: address(encoded)?,
                    txid: *txid,
                    output_index: *index,
                    script: created.output.script.clone(),
                    satoshis: created.output.value,
                    height: created.height,
                })
            })
            .collect()
    }
}

/// `start..=end` checked: refused when inverted or (where `tip_bound`) past
/// the tip.
fn range_ok(served: &Served<'_>, start: Height, end: Height, tip_bound: bool) -> bool {
    let past_tip = |height: Height| {
        served
            .tip_height()
            .is_none_or(|tip| u32::from(height) > tip)
    };
    start <= end && !(tip_bound && (past_tip(start) || past_tip(end)))
}

/// Each transaction in `start..=end` touching the addresses, in chain order,
/// with its height and position.
fn touching(
    served: &Served<'_>,
    ledger: &Ledger,
    wanted: &BTreeSet<String>,
    start: Height,
    end: Height,
) -> Vec<(Height, u32, TransactionId)> {
    let network = served.facts().network;
    let pays = |script: &[u8]| address_of(script, network).is_some_and(|a| wanted.contains(&a));
    served
        .best_chain()
        .filter(|block| block.height() >= start && block.height() <= end)
        .flat_map(|block| {
            (0u32..)
                .zip(block.block.transactions())
                .filter_map(move |(position, tx)| {
                    let credits = tx
                        .transparent
                        .outputs
                        .iter()
                        .any(|out| pays(out.script.as_bytes()));
                    let debits = tx.transparent.inputs.iter().any(|input| {
                        ledger
                            .outputs
                            .get(&(input.prev_txid, input.prev_index))
                            .is_some_and(|created| pays(created.output.script.as_bytes()))
                    });
                    (credits || debits).then_some((block.height(), position, tx.txid))
                })
        })
        .collect()
}

impl zaino_source::OneShotGetAddressTxids for MockChain {
    /// Transactions paying or spending from the addresses in `start..=end`,
    /// in chain order; a range inverted or past the tip is `InvalidRange`.
    async fn get_address_txids(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<TransactionId>, QueryError<GetAddressTxidsError>> {
        let served = self
            .enter(
                "get_address_txids",
                addresses_arg(&addresses, Some((start, end))),
            )
            .await?;
        if !range_ok(&served, start, end, true) {
            return Err(QueryError::Domain(GetAddressTxidsError::InvalidRange {
                start,
                end,
            }));
        }
        let wanted = requested(
            addresses,
            served.facts().network,
            GetAddressTxidsError::InvalidAddress,
        )?;
        let ledger = Ledger::of(&served);
        let txids = touching(&served, &ledger, &wanted, start, end);
        capped(txids.len(), |limit| GetAddressTxidsError::TooManyResults {
            limit,
        })?;
        Ok(txids.into_iter().map(|(_, _, txid)| txid).collect())
    }
}

impl zaino_source::OneShotGetAddressDeltas for MockChain {
    /// Credits (each output paying an address, under the output's index) and
    /// debits (each input spending such an output, its value negated, under
    /// the input's index), by (height, position, index), a credit before a
    /// debit at the same index; an inverted range is `InvalidRange`.
    async fn get_address_deltas(
        &self,
        addresses: Vec<String>,
        start: Height,
        end: Height,
    ) -> Result<Vec<AddressDelta>, QueryError<GetAddressDeltasError>> {
        let served = self
            .enter(
                "get_address_deltas",
                addresses_arg(&addresses, Some((start, end))),
            )
            .await?;
        if !range_ok(&served, start, end, false) {
            return Err(QueryError::Domain(GetAddressDeltasError::InvalidRange {
                start,
                end,
            }));
        }
        let network = served.facts().network;
        let wanted = requested(addresses, network, GetAddressDeltasError::InvalidAddress)?;
        let ledger = Ledger::of(&served);
        let located = touching(&served, &ledger, &wanted, start, end);
        capped(located.len(), |limit| {
            GetAddressDeltasError::TooManyResults { limit }
        })?;
        let mut deltas = Vec::new();
        for (height, position, txid) in located {
            let Some(tx) = served.at(height).and_then(|block| {
                block
                    .block
                    .transactions()
                    .get(usize::try_from(position).ok()?)
            }) else {
                continue;
            };
            let mut push = |index: u32, value: Zatoshis, debit: bool, script: &[u8]| {
                let Some(encoded) = address_of(script, network).filter(|a| wanted.contains(a))
                else {
                    return Ok(());
                };
                let value = i64::try_from(u64::from(value)).map_err(|_| fault("value past i64"))?;
                deltas.push(AddressDelta {
                    satoshis: SignedZatoshis::try_new(if debit { -value } else { value })
                        .map_err(|why| fault(why.to_string()))?,
                    txid,
                    index,
                    height,
                    address: address(encoded)?,
                    block_index: Some(position),
                });
                Ok::<(), QueryError<GetAddressDeltasError>>(())
            };
            for (index, output) in (0u32..).zip(&tx.transparent.outputs) {
                push(index, output.value, false, output.script.as_bytes())?;
            }
            for (index, input) in (0u32..).zip(&tx.transparent.inputs) {
                if let Some(created) = ledger.outputs.get(&(input.prev_txid, input.prev_index)) {
                    push(
                        index,
                        created.output.value,
                        true,
                        created.output.script.as_bytes(),
                    )?;
                }
            }
        }
        // stable: at equal (height, position, index) the credit, pushed first, leads
        deltas.sort_by_key(|delta| (delta.height, delta.block_index, delta.index));
        Ok(deltas)
    }
}
