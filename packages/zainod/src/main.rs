//! Zaino Indexer daemon.

// production code never panics: no indexing, unchecked arithmetic or panicking calls
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
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::let_underscore_must_use,
        let_underscore_drop
    )
)]

use clap::Parser;

use zainodlib::cli::{default_config_path, Cli, Command};

#[tokio::main]
async fn main() {
    if let Err(e) = zaino_logging::init() {
        eprintln!("Error: logging: {e}");
        std::process::exit(1);
    }

    let cli = Cli::parse();

    match cli.command {
        Command::Start { config } => {
            let config_path = config.unwrap_or_else(default_config_path);
            if let Err(e) = zainodlib::run(config_path).await {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        }
        Command::GenerateConfig { output } => Command::generate_config(output),
    }
}
