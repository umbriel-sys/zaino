//! Zaino Indexer service.

#![warn(missing_docs)]
#![forbid(unsafe_code)]
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

use std::path::PathBuf;

use crate::config::load_config;
use crate::error::IndexerError;
use crate::indexer::start_indexer;
use tracing::{error, info};

#[cfg(feature = "prometheus")]
pub(crate) mod admin;
pub mod cli;
pub mod config;
pub mod error;
pub mod indexer;
#[cfg(feature = "prometheus")]
pub mod metrics;

/// Run the Zaino indexer.
///
/// Runs the main indexer loop with restart support.
/// Logging should be initialized by the caller before calling this function.
/// Returns an error if config loading or indexer startup fails.
pub async fn run(config_path: PathBuf) -> Result<(), IndexerError> {
    zaino_logging::try_init();

    info!(version = env!("CARGO_PKG_VERSION"), "zainod started");
    let config = load_config(&config_path)?;

    #[cfg(feature = "prometheus")]
    if let Some(endpoint) = config.metrics_endpoint {
        crate::metrics::init(endpoint)?;
    }

    // Else silent: the block above is compiled out, so a configured endpoint yields
    // no listener and no complaint
    #[cfg(not(feature = "prometheus"))]
    if config.metrics_endpoint.is_some() {
        tracing::warn!(
            "`metrics_endpoint` is configured but this binary was built without the \
             `prometheus` feature, so no /metrics listener will start. Rebuild with \
             `--features prometheus` (or `no_tls_with_prometheus`) to enable it."
        );
    }

    loop {
        match start_indexer(config.clone()).await {
            Ok(joinhandle_result) => {
                info!("Zaino Indexer started successfully.");
                match joinhandle_result.await {
                    Ok(indexer_result) => match indexer_result {
                        Ok(()) => {
                            info!("Exiting Zaino successfully.");
                            return Ok(());
                        }
                        Err(IndexerError::Restart) => {
                            error!("Zaino encountered critical error, restarting.");
                            // The recorder outlives this loop, so nothing else resets:
                            // a crash-looping indexer scrapes like a healthy one
                            #[cfg(feature = "prometheus")]
                            {
                                crate::metrics::record_restart();
                                crate::admin::clear_heartbeat();
                            }
                            continue;
                        }
                        Err(e) => {
                            error!(%e, "exiting Zaino with error");
                            return Err(e);
                        }
                    },
                    Err(e) => {
                        error!(%e, "Zaino exited early with error");
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                error!(%e, "Zaino failed to start");
                return Err(e);
            }
        }
    }
}
