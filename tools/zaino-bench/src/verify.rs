//! `verify`: a store's read ports against the source at sampled heights.

use std::collections::HashMap;
use std::fmt;

use zaino_chain_store::{
    ChainStoreReader, CompactBlockRead, PoolFilter, SpentOutputIndex, StoredHeader,
    TransactionIndex, TreeRootsIndex, TreeStateIndex, TxOutSetIndex,
};
use zaino_indexes::frontier_from_legacy_tree;
use zaino_primitives::types::{
    ChainMetadata, Height, Outpoint, PoolFrontier, PreIndexCompactTx, ShieldedPool, TransactionId,
    TreeRootInfo, TreeSize,
};
use zaino_source::{
    OneShotGetBlock as _, OneShotGetCommitmentTreeRoots as _, OneShotGetTreestate as _,
};

use crate::stores::Source;

/// What was checked and what disagreed.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Report {
    pub watermark: Option<u32>,
    pub heights_checked: u32,
    pub transactions_checked: u32,
    pub outpoints_checked: u32,
    /// Outputs the store says were spent further above their block than
    /// `--spend-scan` looked: neither confirmed nor contradicted.
    pub spenders_unverified: u32,
    pub mismatches: Vec<String>,
    pub txout_set: Option<TxOutSetSummary>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct TxOutSetSummary {
    pub transactions: u64,
    pub transaction_outputs: u64,
    pub total_zatoshis: u64,
    pub hash_serialized: String,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
    }

    fn note(&mut self, what: impl Into<String>) {
        let what = what.into();
        eprintln!("  mismatch: {what}");
        self.mismatches.push(what);
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "verify: watermark {:?}; {} heights, {} transactions, {} outpoints checked ({} spenders beyond --spend-scan, unverified); {} mismatches",
            self.watermark,
            self.heights_checked,
            self.transactions_checked,
            self.outpoints_checked,
            self.spenders_unverified,
            self.mismatches.len()
        )?;
        if let Some(set) = &self.txout_set {
            writeln!(
                f,
                "  txout set at the watermark: {} transactions, {} outputs, {} zatoshis, {}",
                set.transactions, set.transaction_outputs, set.total_zatoshis, set.hash_serialized
            )?;
        }
        Ok(())
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("a chain height")
}

/// The heights to check: one in `every`, plus the first and last fifty.
pub fn sampled(watermark: u32, every: u32) -> Vec<u32> {
    let every = every.max(1);
    let mut heights: Vec<u32> = (0..=watermark).step_by(every as usize).collect();
    heights.extend(0..=watermark.min(50));
    heights.extend(watermark.saturating_sub(50)..=watermark);
    heights.sort_unstable();
    heights.dedup();
    heights
}

/// `spend_scan`: blocks read above each sample for its outputs' spenders (0
/// skips the spender check).
pub async fn run<R>(
    reader: &R,
    source: &Source,
    every: u32,
    spend_scan: u32,
) -> Result<Report, String>
where
    R: ChainStoreReader
        + CompactBlockRead
        + TreeRootsIndex
        + TransactionIndex
        + SpentOutputIndex
        + TxOutSetIndex,
{
    let mut report = Report::default();
    let Some(tip) = reader.watermark().tip else {
        return Err("the store is empty".to_owned());
    };
    let watermark = u32::from(tip.height);
    report.watermark = Some(watermark);
    let (created, spenders) = ledger(source, watermark, every, spend_scan).await?;

    for h in sampled(watermark, every) {
        let at = height(h);
        let block = source
            .get_block(at)
            .await
            .map_err(|error| format!("source block {h}: {error}"))?;
        let roots = source
            .get_commitment_tree_roots(block.header.hash)
            .await
            .map_err(|error| format!("source roots {h}: {error}"))?;
        report.heights_checked += 1;

        match reader.block_hash(at).await {
            Ok(Some(hash)) if hash == block.header.hash => {}
            other => report.note(format!(
                "block_hash({h}) = {other:?}, chain {}",
                block.header.hash
            )),
        }
        match reader.block_height(block.header.hash).await {
            Ok(Some(height)) if height == at => {}
            other => report.note(format!("block_height({h}) = {other:?}")),
        }
        match reader.headers_chunk(at, at).await.as_deref() {
            Ok([StoredHeader { header, .. }]) if *header == block.header => {}
            other => report.note(format!("headers_chunk({h}) = {other:?}")),
        }
        match reader
            .compact_chunk(at, at, PoolFilter::all())
            .await
            .as_deref()
        {
            Ok([compact]) => {
                let expected: Vec<PreIndexCompactTx> = block
                    .transactions()
                    .iter()
                    .map(PreIndexCompactTx::from)
                    .collect();
                if compact.hash != block.header.hash || compact.transactions != expected {
                    report.note(format!("compact_chunk({h}) differs from the chain's block"));
                }
                // The adapters leave a block's `chain_metadata` zero (indexed state, not block
                // data): the sizes come from the roots.
                let size = |info: &Option<TreeRootInfo>| {
                    info.as_ref().map_or(TreeSize::ZERO, |info| info.size)
                };
                let expected = ChainMetadata {
                    sapling_tree_size: size(&roots.sapling),
                    orchard_tree_size: size(&roots.orchard),
                    ironwood_tree_size: size(&roots.ironwood),
                };
                if compact.chain_metadata != expected {
                    report.note(format!(
                        "compact_chunk({h}) metadata {:?} vs the chain's roots {expected:?}",
                        compact.chain_metadata
                    ));
                }
            }
            other => report.note(format!("compact_chunk({h}) = {other:?}")),
        }
        match reader.tree_roots_chunk(at, at).await.as_deref() {
            Ok([stored]) if *stored == roots => {}
            other => report.note(format!(
                "tree_roots_chunk({h}) = {other:?}, chain {roots:?}"
            )),
        }
        for (index, tx) in block.transactions().iter().enumerate() {
            report.transactions_checked += 1;
            let position = zaino_primitives::types::BlockTxPosition::new(at, index as u32);
            match reader.tx_position(&tx.txid).await {
                Ok(Some(found)) if found == position => {}
                other => report.note(format!("tx_position({h}:{index}) = {other:?}")),
            }
            match reader.txid_at(position).await {
                Ok(Some(found)) if found == tx.txid => {}
                other => report.note(format!("txid_at({h}:{index}) = {other:?}")),
            }
        }
    }

    // Spends: who spent each sampled block's outputs, from the chain itself; a spend the
    // capped scan could not see (further than `spend_scan` above the output) is unverified.
    for chunk in created.chunks(512) {
        report.outpoints_checked += chunk.len() as u32;
        let outpoints: Vec<Outpoint> = chunk.iter().map(|(outpoint, _)| *outpoint).collect();
        let found = reader
            .outpoint_spenders(&outpoints)
            .await
            .map_err(|error| format!("outpoint_spenders: {error}"))?;
        for ((outpoint, created_at), spender) in chunk.iter().zip(found) {
            let expected = spenders.get(outpoint).copied();
            let beyond_scan = spender.as_ref().is_some_and(|spender| {
                u32::from(spender.position.height) > created_at.saturating_add(spend_scan)
            });
            if expected.is_none() && beyond_scan {
                report.spenders_unverified += 1;
            } else if spender.as_ref().map(|spender| spender.txid) != expected {
                report.note(format!(
                    "outpoint_spenders({outpoint:?}) = {spender:?}, chain {expected:?}"
                ));
            }
        }
    }

    match reader.txout_set().await {
        Ok(set) => {
            report.txout_set = Some(TxOutSetSummary {
                transactions: set.transactions,
                transaction_outputs: set.transaction_outputs,
                total_zatoshis: set.total_zatoshis,
                hash_serialized: set
                    .hash_serialized
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect(),
            })
        }
        Err(error) => println!("  txout set not available: {error}"),
    }
    Ok(report)
}

/// The sampled blocks' outputs and who spent them within `spend_scan` blocks
/// (none when `spend_scan` is 0: the spender check is skipped).
async fn ledger(
    source: &Source,
    watermark: u32,
    every: u32,
    spend_scan: u32,
) -> Result<(Vec<(Outpoint, u32)>, HashMap<Outpoint, TransactionId>), String> {
    if spend_scan == 0 {
        return Ok((Vec::new(), HashMap::new()));
    }
    let sampled = sampled(watermark, every);
    let mut created: Vec<(Outpoint, u32)> = Vec::new();
    for h in &sampled {
        let block = source
            .get_block(height(*h))
            .await
            .map_err(|error| format!("source block {h}: {error}"))?;
        for tx in block.transactions() {
            for index in 0..tx.transparent.outputs.len() {
                created.push((
                    Outpoint {
                        txid: tx.txid,
                        index: index as u32,
                    },
                    *h,
                ));
            }
        }
    }
    // Scanning the whole chain for spenders is a full read of the source, so
    // the scan is capped at `spend_scan` blocks above each sample; a spend
    // the store reports further up is counted unverified by the caller.
    let wanted: std::collections::HashSet<Outpoint> =
        created.iter().map(|(outpoint, _)| *outpoint).collect();
    let mut spenders = HashMap::new();
    let mut scanned = std::collections::HashSet::new();
    for h in &sampled {
        for at in *h..=h.saturating_add(spend_scan).min(watermark) {
            if !scanned.insert(at) {
                continue;
            }
            let block = source
                .get_block(height(at))
                .await
                .map_err(|error| format!("source block {at}: {error}"))?;
            for tx in block.transactions() {
                for input in &tx.transparent.inputs {
                    let outpoint = Outpoint {
                        txid: input.prev_txid,
                        index: input.prev_index,
                    };
                    if wanted.contains(&outpoint) {
                        spenders.insert(outpoint, tx.txid);
                    }
                }
            }
        }
    }
    Ok((created, spenders))
}

/// C2: the store's frontiers against the validator's serialized trees.
pub async fn run_tree_state<R: ChainStoreReader + TreeStateIndex>(
    reader: &R,
    source: &Source,
    every: u32,
    report: &mut Report,
) -> Result<(), String> {
    let Some(watermark) = report.watermark else {
        return Ok(());
    };
    for h in sampled(watermark, every) {
        let at = height(h);
        let treestate = source
            .get_treestate(at)
            .await
            .map_err(|error| format!("source treestate {h}: {error}"))?;
        for pool in ShieldedPool::ALL {
            let tree = match pool {
                ShieldedPool::Sapling => treestate.sapling.as_ref(),
                ShieldedPool::Orchard => treestate.orchard.as_ref(),
                ShieldedPool::Ironwood => treestate.ironwood.as_ref(),
            };
            let expected = match tree {
                None => PoolFrontier::Absent,
                Some(tree) => match frontier_from_legacy_tree(&tree.final_state) {
                    Ok(None) => PoolFrontier::Empty,
                    Ok(Some(frontier)) => PoolFrontier::Frontier(frontier),
                    Err(error) => {
                        report.note(format!("validator tree {pool} at {h} undecodable: {error}"));
                        continue;
                    }
                },
            };
            match reader.pool_frontier(pool, at).await {
                Ok(Some(found)) if found == expected => {}
                Err(zaino_chain_store::ChainStoreError::Unavailable(_)) => return Ok(()),
                other => report.note(format!("pool_frontier({pool}, {h}) = {other:?}")),
            }
        }
    }
    Ok(())
}
