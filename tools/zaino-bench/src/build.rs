//! `build`: time `build_to` through the ingest port, sampling the watermark.

use std::fmt;
use std::time::{Duration, Instant};

use zaino_chain_store::{ChainStoreIngest, ChainStoreService};
use zaino_primitives::types::Height;

/// One watermark sample.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Sample {
    pub elapsed_seconds: f64,
    pub height: Option<u32>,
    pub blocks_per_second: f64,
}

/// A build's outcome.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Outcome {
    pub from: Option<u32>,
    pub to: u32,
    pub reached: Option<u32>,
    pub seconds: f64,
    pub blocks_per_second: f64,
    pub samples: Vec<Sample>,
    pub error: Option<String>,
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "build: {:?} -> {} reached {:?} in {:.1}s ({:.0} blocks/s){}",
            self.from,
            self.to,
            self.reached,
            self.seconds,
            self.blocks_per_second,
            self.error
                .as_ref()
                .map_or(String::new(), |error| format!("; failed: {error}")),
        )
    }
}

/// `error` and every cause under it, on one line.
fn causes(error: &dyn std::error::Error) -> String {
    let mut line = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        line.push_str(": ");
        line.push_str(&cause.to_string());
        source = cause.source();
    }
    line
}

fn tip_height(watermark: &zaino_chain_store::StoreWatermark) -> Option<u32> {
    watermark.tip.map(|tip| u32::from(tip.height))
}

pub async fn run<S: ChainStoreService + ChainStoreIngest>(
    store: &S,
    to: u32,
    sample_seconds: u64,
) -> Result<Outcome, String> {
    let target = Height::try_from(to).map_err(|error| format!("height {to}: {error}"))?;
    let mut watermark = store.subscribe_watermark();
    let from = tip_height(&watermark.borrow());
    println!("building from {from:?} through {to}; sampling every {sample_seconds}s");

    let started = Instant::now();
    let mut samples = Vec::new();
    let mut last = (started, from.map_or(0, |h| h + 1));
    let mut build = std::pin::pin!(store.build_to(target));
    let mut ticker = tokio::time::interval(Duration::from_secs(sample_seconds.max(1)));
    ticker.tick().await;
    let result = loop {
        tokio::select! {
            result = &mut build => break result,
            _ = ticker.tick() => {
                let now = Instant::now();
                let height = tip_height(&watermark.borrow_and_update());
                let built = height.map_or(0, |h| h + 1).saturating_sub(last.1);
                let rate = built as f64 / now.duration_since(last.0).as_secs_f64();
                println!(
                    "  {:>7.0}s  height {:>9}  {:>7.0} blocks/s",
                    now.duration_since(started).as_secs_f64(),
                    height.map_or("-".to_owned(), |h| h.to_string()),
                    rate,
                );
                samples.push(Sample {
                    elapsed_seconds: now.duration_since(started).as_secs_f64(),
                    height,
                    blocks_per_second: rate,
                });
                last = (now, height.map_or(0, |h| h + 1));
            }
        }
    };
    let seconds = started.elapsed().as_secs_f64();
    let reached = tip_height(&watermark.borrow());
    let built = reached
        .map_or(0, |h| h + 1)
        .saturating_sub(from.map_or(0, |h| h + 1));
    let outcome = Outcome {
        from,
        to,
        reached,
        seconds,
        blocks_per_second: built as f64 / seconds.max(1e-9),
        samples,
        error: result.err().map(|error| causes(&error)),
    };
    println!("{outcome}");
    Ok(outcome)
}
