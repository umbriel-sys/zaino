//! zaino-bench: runs the zainod binary on a config, times its sync to the
//! tip, and reports where the time went.

// the tool never panics: no indexing or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::let_underscore_must_use
    )
)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};

mod metrics;
mod record;
mod report;
mod run;

use record::Results;

#[derive(Parser)]
#[command(about = "Times a zainod sync and reports where the time went")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Sync zainod on a config, from an empty data directory to the tip.
    Run {
        /// The zainod config, used exactly as written. Its `[persistence]
        /// path` must be empty and its `[admin]` endpoint set.
        #[arg(long)]
        config: PathBuf,
        /// A prebuilt zainod (with the `prometheus` feature). Default: build
        /// this checkout's.
        #[arg(long)]
        zainod: Option<PathBuf>,
        /// The longest the sync may take: `90m`, `48h`.
        #[arg(long, default_value = "48h", value_parser = parse_duration)]
        timeout: Duration,
    },
    /// Show every recorded run, and write the HTML report.
    Report {
        /// Where the HTML report is written. Default: `report.html` in the
        /// results directory.
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

/// A whole number of seconds, minutes, hours or days: `30s`, `90m`, `48h`, `2d`.
fn parse_duration(text: &str) -> Result<Duration, String> {
    let invalid = || format!("`{text}` is not a duration such as 30s, 90m, 48h or 2d");
    let unit = text.chars().last().ok_or_else(invalid)?;
    let per_unit: u64 = match unit {
        's' => 1,
        'm' => 60,
        'h' => 3_600,
        'd' => 86_400,
        _ => return Err(invalid()),
    };
    text.strip_suffix(unit)
        .and_then(|number| number.parse::<u64>().ok())
        .and_then(|number| number.checked_mul(per_unit))
        .filter(|secs| *secs > 0)
        .map(Duration::from_secs)
        .ok_or_else(invalid)
}

fn report(results: &Results, output: Option<PathBuf>) -> Result<(), String> {
    let records = results.records().map_err(|error| error.to_string())?;
    print!("{}", report::runs_table(&records));
    if records.is_empty() {
        return Ok(());
    }
    let mut runs = Vec::new();
    for record in records {
        let series = results
            .series(&record.run_id)
            .map_err(|error| error.to_string())?;
        runs.push((record, series));
    }
    let output = output.unwrap_or_else(|| results.root().join("report.html"));
    std::fs::write(&output, report::html(&runs))
        .map_err(|error| format!("{}: {error}", output.display()))?;
    println!("\nHTML report: {}", output.display());
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let results = Results::default_dir();
    let outcome = match cli.command {
        Cmd::Run {
            config,
            zainod,
            timeout,
        } => run::run(&config, zainod, timeout, &results)
            .await
            .map_err(|error| error.to_string()),
        Cmd::Report { output } => report(&results, output).map(|()| true),
    };
    match outcome {
        Ok(true) => ExitCode::SUCCESS,
        // the run failed and is recorded as failed
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("zaino-bench: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_are_whole_numbers_with_a_unit() {
        assert_eq!(parse_duration("30s"), Ok(Duration::from_secs(30)));
        assert_eq!(parse_duration("90m"), Ok(Duration::from_secs(5_400)));
        assert_eq!(parse_duration("48h"), Ok(Duration::from_secs(172_800)));
        assert_eq!(parse_duration("2d"), Ok(Duration::from_secs(172_800)));
        for invalid in [
            "",
            "h",
            "48",
            "1.5h",
            "-2h",
            "0h",
            "10 m",
            "99999999999999999999d",
        ] {
            assert!(parse_duration(invalid).is_err(), "{invalid}");
        }
    }
}
