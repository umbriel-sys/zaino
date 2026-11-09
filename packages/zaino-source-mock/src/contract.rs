//! The source contract (D-88): what every source answers, checked through
//! the ports alone.
//!
//! Each check discovers the chain through the source it checks, so the same
//! checks run against [`MockChain`](crate::MockChain) in fast tests and
//! against zebrad (read-state and RPC) and zcashd in the live suite. A
//! check returns the first rule it found broken.

use std::collections::BTreeSet;

use zaino_primitives::types::{
    rpc::{ChainTipStatus, SpentOutpoint},
    Block, BlockHash, Height, ShieldedPool, TransactionId, TransactionLocation,
};
use zaino_source::{
    GetAddressBalanceError, GetAddressDeltasError, GetAddressTxidsError, GetAddressUtxosError,
    GetBlockByHashError, GetBlockError, GetCommitmentTreeRootsByHeightError,
    GetCommitmentTreeRootsError, GetSpentInfoError, GetSubtreeRootsError, GetTransactionError,
    GetTreestateByHashError, GetTreestateError, QueryError,
};

/// A broken rule, named.
pub type Broken = String;

/// A hash no chain holds.
const UNKNOWN: [u8; 32] = [0xee; 32];

fn ensure(holds: bool, rule: impl FnOnce() -> String) -> Result<(), Broken> {
    match holds {
        true => Ok(()),
        false => Err(rule()),
    }
}

fn answered<T, E: std::fmt::Debug + std::fmt::Display, N: std::error::Error>(
    what: &str,
    result: Result<T, QueryError<E, N>>,
) -> Result<T, Broken> {
    result.map_err(|error| format!("{what}: {error}"))
}

/// The best tip's height.
async fn tip<S: zaino_source::OneShotGetBestBlockHeight>(source: &S) -> Result<Height, Broken> {
    answered("best block height", source.get_best_block_height().await)
}

/// Blocks by height and by hash agree and link; past the tip and an unknown
/// hash are the domain refusals; the tip ports agree.
pub async fn blocks<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetBlockByHash
        + zaino_source::OneShotGetChainTip
        + zaino_source::OneShotGetChainTips,
{
    let tip = tip(source).await?;
    let mut previous: Option<Block> = None;
    for height in 0..=u32::from(tip) {
        let height = Height::try_from(height).map_err(|e| e.to_string())?;
        let block = answered("block by height", source.get_block(height).await)?;
        ensure(block.header.height == height, || {
            format!("block {height} reports height {}", block.header.height)
        })?;
        let by_hash = answered(
            "block by hash",
            source.get_block_by_hash(block.header.hash).await,
        )?;
        ensure(by_hash.header == block.header, || {
            format!("block {height} by hash differs")
        })?;
        if let Some(previous) = &previous {
            ensure(block.header.prev_hash == previous.header.hash, || {
                format!("block {height} does not link")
            })?;
        }
        previous = Some(block);
    }
    let past = tip.checked_add(1).ok_or("tip at the maximum height")?;
    ensure(
        matches!(source.get_block(past).await, Err(QueryError::Domain(GetBlockError::HeightNotFound(h))) if h == past),
        || "a height past the tip is not HeightNotFound".to_owned(),
    )?;
    let unknown = BlockHash::from(UNKNOWN);
    ensure(
        matches!(
            source.get_block_by_hash(unknown).await,
            Err(QueryError::Domain(GetBlockByHashError::NotFound(_)))
        ),
        || "an unknown hash is not NotFound".to_owned(),
    )?;
    let (hash, height) = answered("chain tip", source.get_chain_tip().await)?;
    let tip_hash = previous.map(|block| block.header.hash);
    ensure(height == tip && Some(hash) == tip_hash, || {
        "chain tip disagrees with the best block".to_owned()
    })?;
    let tips = answered("chain tips", source.get_chain_tips().await)?;
    ensure(
        tips.iter()
            .filter(|t| t.status == ChainTipStatus::Active)
            .map(|t| (t.hash, t.height))
            .eq([(hash, height)]),
        || "chain tips do not name the best tip as the one active tip".to_owned(),
    )
}

/// Roots by hash and by height agree; an unknown hash is `BlockNotFound`, a
/// height past the tip `HeightNotFound`.
pub async fn tree_roots<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetCommitmentTreeRoots
        + zaino_source::OneShotGetCommitmentTreeRootsByHeight,
{
    let tip = tip(source).await?;
    for height in 0..=u32::from(tip) {
        let height = Height::try_from(height).map_err(|e| e.to_string())?;
        let hash = answered("block", source.get_block(height).await)?
            .header
            .hash;
        let (named, by_height) = answered(
            "roots by height",
            source.get_commitment_tree_roots_by_height(height).await,
        )?;
        ensure(named == hash, || {
            format!("roots at {height} name another block")
        })?;
        let by_hash = answered(
            "roots by hash",
            source.get_commitment_tree_roots(hash).await,
        )?;
        ensure(by_hash == by_height, || {
            format!("roots at {height} differ by hash and height")
        })?;
    }
    ensure(
        matches!(
            source
                .get_commitment_tree_roots(BlockHash::from(UNKNOWN))
                .await,
            Err(QueryError::Domain(
                GetCommitmentTreeRootsError::BlockNotFound(_)
            ))
        ),
        || "roots of an unknown hash are not BlockNotFound".to_owned(),
    )?;
    let past = tip.checked_add(1).ok_or("tip at the maximum height")?;
    ensure(
        matches!(
            source.get_commitment_tree_roots_by_height(past).await,
            Err(QueryError::Domain(
                GetCommitmentTreeRootsByHeightError::HeightNotFound(_)
            ))
        ),
        || "roots past the tip are not HeightNotFound".to_owned(),
    )
}

/// Treestates by height and hash agree and name their block, carry no root
/// (roots come from the roots ports), and refuse an unknown hash and a
/// height past the tip.
pub async fn treestates<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetTreestate
        + zaino_source::OneShotGetTreestateByHash,
{
    let tip = tip(source).await?;
    for height in 0..=u32::from(tip) {
        let height = Height::try_from(height).map_err(|e| e.to_string())?;
        let hash = answered("block", source.get_block(height).await)?
            .header
            .hash;
        let by_height = answered("treestate", source.get_treestate(height).await)?;
        ensure(
            by_height.block_hash == hash && by_height.height == height,
            || format!("treestate at {height} names another block"),
        )?;
        let by_hash = answered(
            "treestate by hash",
            source.get_treestate_by_hash(hash).await,
        )?;
        ensure(by_hash == by_height, || {
            format!("treestate at {height} differs by hash and height")
        })?;
        let rooted = [&by_height.sapling, &by_height.orchard, &by_height.ironwood]
            .into_iter()
            .flatten()
            .any(|pool| pool.final_root.is_some());
        ensure(!rooted, || format!("treestate at {height} carries a root"))?;
    }
    ensure(
        matches!(
            source.get_treestate_by_hash(BlockHash::from(UNKNOWN)).await,
            Err(QueryError::Domain(GetTreestateByHashError::BlockNotFound(
                _
            )))
        ),
        || "the treestate of an unknown hash is not BlockNotFound".to_owned(),
    )?;
    let past = tip.checked_add(1).ok_or("tip at the maximum height")?;
    ensure(
        matches!(
            source.get_treestate(past).await,
            Err(QueryError::Domain(GetTreestateError::HeightNotFound(_)))
        ),
        || "a treestate past the tip is not HeightNotFound".to_owned(),
    )
}

/// Every mined transaction is located on the best chain at its block's
/// height; an unknown txid is `NotFound`.
pub async fn transactions<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetTransaction,
{
    let tip = tip(source).await?;
    for height in 0..=u32::from(tip) {
        let height = Height::try_from(height).map_err(|e| e.to_string())?;
        for tx in answered("block", source.get_block(height).await)?.transactions() {
            let found = answered("transaction", source.get_transaction(tx.txid).await)?;
            ensure(
                found.location == TransactionLocation::BestChain(height),
                || format!("{} is not located at {height}", tx.txid),
            )?;
        }
    }
    ensure(
        matches!(
            source.get_transaction(TransactionId::from(UNKNOWN)).await,
            Err(QueryError::Domain(GetTransactionError::NotFound(_)))
        ),
        || "an unknown txid is not NotFound".to_owned(),
    )
}

/// Subtree roots end in ascending order at or below the tip, and `limit`
/// and `start_index` bound the answer. Sapling and Orchard are always
/// answered; Ironwood may be `PoolUnavailable` (zcashd lacks it).
pub async fn subtrees<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight + zaino_source::OneShotGetSubtreeRoots,
{
    let tip = tip(source).await?;
    for pool in ShieldedPool::ALL {
        let all = match source.get_subtree_roots(pool, 0, None).await {
            Err(QueryError::Domain(GetSubtreeRootsError::PoolUnavailable(refused)))
                if pool == ShieldedPool::Ironwood && refused == pool =>
            {
                continue;
            }
            answer => answered("subtree roots", answer)?,
        };
        ensure(
            all.windows(2)
                .all(|pair| matches!(pair, [a, b] if a.end_height <= b.end_height))
                && all.iter().all(|root| root.end_height <= tip),
            || format!("{pool} subtrees out of order or past the tip"),
        )?;
        let one = answered(
            "subtree roots",
            source.get_subtree_roots(pool, 0, Some(1)).await,
        )?;
        ensure(one.len() == all.len().min(1), || {
            format!("{pool} subtree limit ignored")
        })?;
        let count = u16::try_from(all.len()).map_err(|e| e.to_string())?;
        let beyond = answered(
            "subtree roots",
            source.get_subtree_roots(pool, count, None).await,
        )?;
        ensure(beyond.is_empty(), || {
            format!("{pool} subtrees past the last answered")
        })?;
    }
    Ok(())
}

/// The address index over `address` (one that has received and spent):
/// deltas carry its credits and its debits, ordered; they sum to its balance
/// and positive ones to its receipts; its UTXOs sum to its balance; its txids
/// are the deltas' transactions in order; each debit's outpoint is spent per
/// `getspentinfo` and absent per `gettxout`, each UTXO present per
/// `gettxout`; a malformed address, and `foreign` (valid, for another
/// network), are `InvalidAddress` on every port.
pub async fn address_index<S>(source: &S, address: &str, foreign: &str) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetAddressBalance
        + zaino_source::OneShotGetAddressDeltas
        + zaino_source::OneShotGetAddressTxids
        + zaino_source::OneShotGetAddressUtxos
        + zaino_source::OneShotGetSpentInfo
        + zaino_source::OneShotGetTxOut,
{
    let tip = tip(source).await?;
    let wanted = vec![address.to_owned()];
    let deltas = answered(
        "deltas",
        source
            .get_address_deltas(wanted.clone(), Height::GENESIS, tip)
            .await,
    )?;
    let debits: Vec<_> = deltas.iter().filter(|d| d.satoshis.as_i64() < 0).collect();
    ensure(!debits.is_empty() && deltas.len() > debits.len(), || {
        format!("{address}: deltas lack credits or debits")
    })?;
    ensure(
        deltas.windows(2).all(|pair| matches!(pair, [a, b] if (a.height, a.block_index, a.index) <= (b.height, b.block_index, b.index))),
        || "deltas out of order".to_owned(),
    )?;
    let balance = answered("balance", source.get_address_balance(wanted.clone()).await)?;
    let net: i128 = deltas.iter().map(|d| i128::from(d.satoshis.as_i64())).sum();
    ensure(net == i128::from(u64::from(balance.balance)), || {
        format!("deltas net {net}, balance {}", u64::from(balance.balance))
    })?;
    let received: u64 = deltas
        .iter()
        .filter(|d| d.satoshis.as_i64() > 0)
        .map(|d| d.satoshis.as_i64().unsigned_abs())
        .sum();
    ensure(
        balance.received == zaino_primitives::types::ZatoshisFlowSum::from_summed(received),
        || "received is not the credits' sum".to_owned(),
    )?;
    let utxos = answered("utxos", source.get_address_utxos(wanted.clone()).await)?;
    let unspent: u64 = utxos.iter().map(|u| u64::from(u.satoshis)).sum();
    ensure(unspent == u64::from(balance.balance), || {
        "UTXOs do not sum to the balance".to_owned()
    })?;
    let txids = answered(
        "txids",
        source
            .get_address_txids(wanted.clone(), Height::GENESIS, tip)
            .await,
    )?;
    let mut seen = BTreeSet::new();
    let from_deltas: Vec<_> = deltas
        .iter()
        .map(|d| d.txid)
        .filter(|txid| seen.insert(*txid))
        .collect();
    ensure(txids == from_deltas, || {
        "txids are not the deltas' transactions in order".to_owned()
    })?;
    for utxo in &utxos {
        let out = answered(
            "txout",
            source.get_tx_out(utxo.txid, utxo.output_index, false).await,
        )?;
        ensure(out.is_some_and(|out| out.value == utxo.satoshis), || {
            format!(
                "UTXO {}:{} absent from gettxout",
                utxo.txid, utxo.output_index
            )
        })?;
    }
    // every credit is either spent, by a debit the deltas carry, or a UTXO
    let mut matched = 0usize;
    for credit in deltas.iter().filter(|d| d.satoshis.as_i64() > 0) {
        let outpoint = SpentOutpoint {
            txid: credit.txid,
            index: credit.index,
        };
        match source.get_spent_info(outpoint).await {
            Ok(spender) => {
                let debited = debits.iter().any(|d| {
                    d.txid == spender.txid && d.index == spender.index && d.height == spender.height
                });
                ensure(debited, || {
                    format!(
                        "{}:{} spent by {}:{}, a debit the deltas lack",
                        credit.txid, credit.index, spender.txid, spender.index
                    )
                })?;
                let out = answered(
                    "txout",
                    source.get_tx_out(credit.txid, credit.index, false).await,
                )?;
                ensure(out.is_none(), || {
                    format!("spent {}:{} present in gettxout", credit.txid, credit.index)
                })?;
                matched += 1;
            }
            Err(QueryError::Domain(GetSpentInfoError::NotSpent)) => {
                let listed = utxos
                    .iter()
                    .any(|u| u.txid == credit.txid && u.output_index == credit.index);
                ensure(listed, || {
                    format!(
                        "unspent {}:{} missing from the UTXOs",
                        credit.txid, credit.index
                    )
                })?;
            }
            Err(error) => return Err(format!("spent info: {error}")),
        }
    }
    ensure(matched == debits.len(), || {
        format!("{} debits, {matched} matched to credits", debits.len())
    })?;
    for invalid in ["not an address", foreign] {
        let invalid = vec![invalid.to_owned()];
        ensure(
            matches!(
                source.get_address_balance(invalid.clone()).await,
                Err(QueryError::Domain(GetAddressBalanceError::InvalidAddress(
                    _
                )))
            ),
            || format!("balance {invalid:?}: InvalidAddress"),
        )?;
        ensure(
            matches!(
                source.get_address_utxos(invalid.clone()).await,
                Err(QueryError::Domain(GetAddressUtxosError::InvalidAddress(_)))
            ),
            || format!("utxos {invalid:?}: InvalidAddress"),
        )?;
        ensure(
            matches!(
                source
                    .get_address_txids(invalid.clone(), Height::GENESIS, tip)
                    .await,
                Err(QueryError::Domain(GetAddressTxidsError::InvalidAddress(_)))
            ),
            || format!("txids {invalid:?}: InvalidAddress"),
        )?;
        ensure(
            matches!(
                source
                    .get_address_deltas(invalid.clone(), Height::GENESIS, tip)
                    .await,
                Err(QueryError::Domain(GetAddressDeltasError::InvalidAddress(_)))
            ),
            || format!("deltas {invalid:?}: InvalidAddress"),
        )?;
    }
    Ok(())
}

/// The mempool listings agree, each member is located in the mempool, and
/// the mempool's tip is at or below the best tip.
pub async fn mempool<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetMempoolTxids
        + zaino_source::OneShotGetMempoolMetadata
        + zaino_source::OneShotGetMempoolSourceTip
        + zaino_source::OneShotGetTransaction,
{
    let tip = tip(source).await?;
    let txids = answered("mempool txids", source.get_mempool_txids().await)?;
    let metadata = answered("mempool metadata", source.get_mempool_metadata().await)?;
    let listed: BTreeSet<_> = txids.iter().collect();
    ensure(listed == metadata.iter().map(|m| &m.txid).collect(), || {
        "mempool listings disagree".to_owned()
    })?;
    for txid in &txids {
        let found = answered("mempool transaction", source.get_transaction(*txid).await)?;
        ensure(found.location == TransactionLocation::Mempool, || {
            format!("{txid} is not located in the mempool")
        })?;
    }
    let (_, at) = answered("mempool source tip", source.get_mempool_source_tip().await)?;
    ensure(at <= tip, || {
        "the mempool's tip is past the best tip".to_owned()
    })
}

/// `getblockchaininfo` names the best tip and its supply is the pools' sum;
/// verbose blocks link forward and count confirmations from the tip.
pub async fn chain_info<S>(source: &S) -> Result<(), Broken>
where
    S: zaino_source::OneShotGetBestBlockHeight
        + zaino_source::OneShotGetBlock
        + zaino_source::OneShotGetBlockchainInfo
        + zaino_source::OneShotGetBlockVerbose,
{
    let tip = tip(source).await?;
    let info = answered("blockchain info", source.get_blockchain_info().await)?;
    let best = answered("block", source.get_block(tip).await)?.header.hash;
    ensure(info.blocks == tip && info.best_block_hash == best, || {
        "blockchain info names another tip".to_owned()
    })?;
    let pooled: u64 = info
        .value_pools
        .iter()
        .map(|pool| u64::from(pool.chain_value))
        .sum();
    ensure(pooled == u64::from(info.chain_supply.chain_value), || {
        "chain supply is not the pools' sum".to_owned()
    })?;
    let mut next: Option<BlockHash> = None;
    for height in (0..=u32::from(tip)).rev() {
        let height = Height::try_from(height).map_err(|e| e.to_string())?;
        let verbose = answered("verbose block", source.get_block_verbose(height).await)?;
        ensure(verbose.next_block_hash == next, || {
            format!("verbose block {height} does not link forward")
        })?;
        let depth = u32::from(tip) - u32::from(height) + 1;
        let counted = matches!(verbose.confirmations, zaino_primitives::types::BlockConfirmations::Confirmed(n) if n.get() == depth);
        ensure(counted, || {
            format!("verbose block {height} miscounts confirmations")
        })?;
        next = Some(
            answered("block", source.get_block(height).await)?
                .header
                .hash,
        );
    }
    Ok(())
}
