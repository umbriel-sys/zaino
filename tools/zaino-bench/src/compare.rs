//! `compare`: two stores' read ports, block by block over a range.

use std::fmt;

use zaino_chain_store::{
    ChainStoreReader, CompactBlockRead, PoolFilter, SpentOutputIndex, TransactionIndex,
    TreeRootsIndex, TxOutSetIndex,
};
use zaino_primitives::types::{Height, Outpoint};

const CHUNK: u32 = 1_000;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Report {
    pub from: u32,
    pub to: u32,
    pub blocks: u32,
    pub differences: Vec<String>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.differences.is_empty()
    }

    fn note(&mut self, what: impl Into<String>) {
        let what = what.into();
        eprintln!("  difference: {what}");
        self.differences.push(what);
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "compare: {}..={} ({} blocks), {} differences",
            self.from,
            self.to,
            self.blocks,
            self.differences.len()
        )
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("a chain height")
}

pub async fn run<A, B>(a: &A, b: &B, from: u32, to: u32) -> Result<Report, String>
where
    A: ChainStoreReader
        + CompactBlockRead
        + TreeRootsIndex
        + TransactionIndex
        + SpentOutputIndex
        + TxOutSetIndex,
    B: ChainStoreReader
        + CompactBlockRead
        + TreeRootsIndex
        + TransactionIndex
        + SpentOutputIndex
        + TxOutSetIndex,
{
    let lowest = [a.watermark().tip, b.watermark().tip]
        .into_iter()
        .map(|tip| tip.map_or(0, |tip| u32::from(tip.height)))
        .min()
        .unwrap_or(0);
    let to = to.min(lowest);
    let mut report = Report {
        from,
        to,
        ..Report::default()
    };
    let mut start = from;
    while start <= to {
        let end = start.saturating_add(CHUNK - 1).min(to);
        let (lo, hi) = (height(start), height(end));
        let headers = (
            a.headers_chunk(lo, hi).await.map_err(|e| e.to_string())?,
            b.headers_chunk(lo, hi).await.map_err(|e| e.to_string())?,
        );
        if headers.0 != headers.1 {
            for (x, y) in headers.0.iter().zip(&headers.1) {
                if x != y {
                    report.note(format!("header at {}: {x:?} vs {y:?}", x.header.height));
                }
            }
            if headers.0.len() != headers.1.len() {
                report.note(format!(
                    "headers {start}..={end}: {} vs {}",
                    headers.0.len(),
                    headers.1.len()
                ));
            }
        }
        let compact = (
            a.compact_chunk(lo, hi, PoolFilter::all())
                .await
                .map_err(|e| e.to_string())?,
            b.compact_chunk(lo, hi, PoolFilter::all())
                .await
                .map_err(|e| e.to_string())?,
        );
        for (x, y) in compact.0.iter().zip(&compact.1) {
            if x != y {
                report.note(format!("compact block at {}", x.height));
            }
        }
        let roots = (
            a.tree_roots_chunk(lo, hi)
                .await
                .map_err(|e| e.to_string())?,
            b.tree_roots_chunk(lo, hi)
                .await
                .map_err(|e| e.to_string())?,
        );
        if roots.0 != roots.1 {
            report.note(format!("tree roots in {start}..={end}"));
        }
        // Positions and spenders of every transaction and output in the chunk.
        let mut outpoints = Vec::new();
        for block in &compact.0 {
            for (index, tx) in block.transactions.iter().enumerate() {
                let (x, y) = (
                    a.tx_position(&tx.txid).await.map_err(|e| e.to_string())?,
                    b.tx_position(&tx.txid).await.map_err(|e| e.to_string())?,
                );
                if x != y {
                    report.note(format!(
                        "tx_position at {}:{index}: {x:?} vs {y:?}",
                        block.height
                    ));
                }
                outpoints.extend((0..tx.transparent_outputs.len()).map(|i| Outpoint {
                    txid: tx.txid,
                    index: i as u32,
                }));
            }
        }
        for chunk in outpoints.chunks(512) {
            let (x, y) = (
                a.outpoint_spenders(chunk)
                    .await
                    .map_err(|e| e.to_string())?,
                b.outpoint_spenders(chunk)
                    .await
                    .map_err(|e| e.to_string())?,
            );
            for ((outpoint, x), y) in chunk.iter().zip(x).zip(y) {
                if x.map(|s| s.txid) != y.map(|s| s.txid) {
                    report.note(format!("spender of {outpoint:?}: {x:?} vs {y:?}"));
                }
            }
        }
        report.blocks += end - start + 1;
        println!(
            "  compared {start}..={end}: {} differences so far",
            report.differences.len()
        );
        start = end.saturating_add(1);
        if end == u32::MAX {
            break;
        }
    }
    if lowest == to {
        match (a.txout_set().await, b.txout_set().await) {
            (Ok(x), Ok(y)) if x != y => report.note(format!("txout set: {x:?} vs {y:?}")),
            (Err(x), _) | (_, Err(x)) => println!("  txout set not compared: {x}"),
            _ => {}
        }
    }
    Ok(report)
}
