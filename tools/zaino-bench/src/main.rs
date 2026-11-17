//! Benchmarks and checks of the chain store, through the `zaino-chain-store`
//! ports alone: any `ChainStoreSource` feeds a store implementing
//! `ChainStoreService + ChainStoreIngest`, and every read goes through the
//! read ports. Wired: zebra's finalized state as the source, the new store
//! on the LSM engine.
//!
//! The figures this prints are working notes (master plan, 5a step 6); the
//! numbers that get published come from the final validation against a live
//! zebrad (Stage 8).

mod build;
mod read;
mod record;
mod recorder;
mod stores;
mod verify;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};

/// Chain-store benchmarks and checks over the store ports.
#[derive(Parser)]
#[command(name = "zaino-bench")]
struct Cli {
    /// Zebra's cache root (the directory holding `state/`), opened read-only.
    #[arg(long)]
    cache: PathBuf,
    /// Where the tool builds the store: `<store-dir>/lsm/`.
    #[arg(long, default_value = "/mnt/framework_ssd/.cache/zaino-bench")]
    store_dir: PathBuf,
    /// Where one JSON record per run is appended.
    #[arg(long, default_value = "bench-results")]
    results: PathBuf,
    /// Rayon threads per LSM index (default: the store shares the machine's out, at least 2).
    #[arg(long)]
    lsm_threads: Option<std::num::NonZeroUsize>,
    /// Each LSM index's write buffer, MiB (the heap at which a commit writes to disk).
    #[arg(long, default_value_t = 64)]
    lsm_write_buffer_mib: usize,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Build the store through a height, sampling the watermark for the rate,
    /// and print the store's metrics at the end.
    Build {
        /// The index set (`full`, `lightclient`, `zcash_rpc`).
        #[arg(long, default_value = "full")]
        set: String,
        /// The height to build through.
        #[arg(long)]
        to: u32,
        /// Seconds between watermark samples.
        #[arg(long, default_value_t = 10)]
        sample: u64,
        /// Start from an empty store directory.
        #[arg(long)]
        fresh: bool,
    },
    /// Check the built store's read ports against the source at sampled heights.
    Verify {
        #[arg(long, default_value = "full")]
        set: String,
        /// Check one height in every `every` (plus the first and last 50).
        #[arg(long, default_value_t = 1000)]
        every: u32,
        /// Blocks read above each sampled height for its outputs' spenders
        /// (every block in the range, so it bounds the run); 0 skips spenders.
        #[arg(long, default_value_t = 20_000)]
        spend_scan: u32,
    },
    /// Read throughput and latency through the built store's reader.
    Read {
        #[arg(long, default_value = "full")]
        set: String,
        #[arg(long, default_value_t = 0)]
        from: u32,
        #[arg(long)]
        to: u32,
    },
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("zaino-bench: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let snapshotter = recorder::install()?;
    let source = Arc::new(stores::open_source(&cli.cache)?);
    let schedule = stores::schedule(source.as_ref()).await?;
    let results = record::Results::new(&cli.results)?;
    let open = |set: &str, fresh: bool| {
        let dir = stores::store_path(&cli.store_dir, fresh);
        let set = set.to_owned();
        let (source, schedule) = (Arc::clone(&source), Arc::clone(&schedule));
        async move {
            stores::open(
                &dir?,
                &set,
                &source,
                &schedule,
                cli.lsm_threads,
                cli.lsm_write_buffer_mib,
            )
            .await
        }
    };

    match cli.command {
        Command::Build {
            set,
            to,
            sample,
            fresh,
        } => {
            let store = open(&set, fresh).await?;
            let outcome = build::run(&store, to, sample).await?;
            let metrics = snapshotter.snapshot();
            print!("{metrics}");
            let mut record = record::Record::build(&set, to, &outcome, &metrics);
            record.lsm_threads = cli.lsm_threads.map(std::num::NonZeroUsize::get);
            record.lsm_write_buffer_mib = Some(cli.lsm_write_buffer_mib);
            results.append(&record)?;
        }
        Command::Verify {
            set,
            every,
            spend_scan,
        } => {
            let store = open(&set, false).await?;
            let reader = zaino_chain_store::ChainStoreService::reader(&store);
            let mut report = verify::run(&reader, source.as_ref(), every, spend_scan).await?;
            verify::run_tree_state(&reader, source.as_ref(), every, &mut report).await?;
            println!("{report}");
            results.append(&record::Record::verify(&set, &report))?;
            if !report.is_clean() {
                return Err(format!("{} mismatches", report.mismatches.len()));
            }
        }
        Command::Read { set, from, to } => {
            let store = open(&set, false).await?;
            let outcome = read::run(
                &zaino_chain_store::ChainStoreService::reader(&store),
                from,
                to,
            )
            .await?;
            println!("{outcome}");
            let metrics = snapshotter.snapshot();
            print!("{metrics}");
            results.append(&record::Record::read(&set, from, to, &outcome, &metrics))?;
        }
    }
    Ok(())
}
