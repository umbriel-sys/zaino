//! `read`: throughput of the compact stream and latency of point reads
//! through a store's reader.

use std::fmt;
use std::time::Instant;

use futures::TryStreamExt as _;
use zaino_chain_store::{ChainStoreReader, CompactBlockRead, PoolFilter, TransactionIndex};
use zaino_primitives::types::Height;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Outcome {
    pub from: u32,
    pub to: u32,
    pub compact_blocks: u64,
    pub compact_seconds: f64,
    pub compact_blocks_per_second: f64,
    pub compact_bytes: u64,
    pub point_reads: u64,
    pub point_mean_micros: f64,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "read: compact stream {}..={}: {} blocks in {:.1}s ({:.0} blocks/s, ~{} MB); {} point reads, mean {:.1}µs",
            self.from,
            self.to,
            self.compact_blocks,
            self.compact_seconds,
            self.compact_blocks_per_second,
            self.compact_bytes >> 20,
            self.point_reads,
            self.point_mean_micros,
        )
    }
}

fn height(h: u32) -> Height {
    Height::try_from(h).expect("a chain height")
}

/// A compact block's payload, roughly.
fn bytes_of(block: &zaino_primitives::types::CompactBlock) -> u64 {
    block
        .transactions
        .iter()
        .map(|tx| {
            32 + 36 * tx.transparent_inputs.len()
                + tx.transparent_outputs
                    .iter()
                    .map(|o| 8 + o.script.as_bytes().len())
                    .sum::<usize>()
                + 32 * tx.sapling_nullifiers.len()
                + 116 * tx.sapling_outputs.len()
                + 148 * (tx.orchard_actions.len() + tx.ironwood_actions.len())
        })
        .sum::<usize>() as u64
        + 80
}

pub async fn run<R>(reader: &R, from: u32, to: u32) -> Result<Outcome, String>
where
    R: ChainStoreReader + CompactBlockRead + TransactionIndex,
{
    let Some(tip) = reader.watermark().tip else {
        return Err("the store is empty".to_owned());
    };
    let to = to.min(u32::from(tip.height));
    let mut outcome = Outcome {
        from,
        to,
        ..Outcome::default()
    };

    let started = Instant::now();
    let stream = reader
        .compact_stream(height(from), height(to), PoolFilter::all())
        .await
        .map_err(|e| e.to_string())?;
    let mut stream = std::pin::pin!(stream);
    let mut txids = Vec::new();
    while let Some(chunk) = stream.try_next().await.map_err(|e| e.to_string())? {
        for block in &chunk {
            outcome.compact_bytes += bytes_of(block);
            if block.height % 97 == 0 {
                if let Some(tx) = block.transactions.first() {
                    txids.push(tx.txid);
                }
            }
        }
        outcome.compact_blocks += chunk.len() as u64;
    }
    outcome.compact_seconds = started.elapsed().as_secs_f64();
    outcome.compact_blocks_per_second =
        outcome.compact_blocks as f64 / outcome.compact_seconds.max(1e-9);

    let started = Instant::now();
    for txid in &txids {
        reader.tx_position(txid).await.map_err(|e| e.to_string())?;
        outcome.point_reads += 1;
    }
    for h in (from..=to).step_by(97) {
        reader
            .block_hash(height(h))
            .await
            .map_err(|e| e.to_string())?;
        outcome.point_reads += 1;
    }
    outcome.point_mean_micros =
        started.elapsed().as_secs_f64() * 1e6 / outcome.point_reads.max(1) as f64;
    Ok(outcome)
}
