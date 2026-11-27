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
use zaino_component::error_chain;

#[cfg(feature = "prometheus")]
pub(crate) mod admin;
pub mod cli;
pub mod config;
pub mod error;
pub mod indexer;
#[cfg(feature = "prometheus")]
pub mod metrics;
pub mod serve;
mod source;

/// Restarts in a row, none of the runs lasting [`STABLE_RUN`], before Zaino
/// exits.
const MAX_RESTARTS: u32 = 5;
/// The wait before the first restart; doubled each restart.
const RESTART_FIRST: std::time::Duration = std::time::Duration::from_secs(5);
/// The longest wait before a restart.
const RESTART_MAX: std::time::Duration = std::time::Duration::from_secs(60);
/// A run this long has recovered: the next failure starts a new count.
const STABLE_RUN: std::time::Duration = std::time::Duration::from_secs(600);

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
    if let Some(admin) = config.admin {
        crate::metrics::init(admin.listen_address)?;
    }

    // Else silent: the block above is compiled out, so a configured endpoint yields
    // no listener and no complaint
    #[cfg(not(feature = "prometheus"))]
    if config.admin.is_some() {
        tracing::warn!(
            "`[admin]` is configured but this binary was built without the `prometheus` \
             feature, so no admin listener will start. Rebuild with `--features prometheus` \
             (or `no_tls_with_prometheus`) to enable it."
        );
    }

    let mut restarts = 0u32;
    let mut backoff = RESTART_FIRST;
    loop {
        match start_indexer(config.clone()).await {
            Ok(joinhandle_result) => {
                info!("Zaino Indexer started successfully.");
                let started = std::time::Instant::now();
                match joinhandle_result.await {
                    Ok(indexer_result) => match indexer_result {
                        Ok(()) => {
                            info!("Exiting Zaino successfully.");
                            return Ok(());
                        }
                        Err(IndexerError::Restart(cause)) => {
                            // a run that held is not part of a restart loop
                            if started.elapsed() >= STABLE_RUN {
                                restarts = 0;
                                backoff = RESTART_FIRST;
                            }
                            restarts = restarts.saturating_add(1);
                            if restarts > MAX_RESTARTS {
                                let failure = IndexerError::Critical(format!(
                                    "gave up after {MAX_RESTARTS} restarts: {cause}"
                                ));
                                error!(%failure, "exiting Zaino with error");
                                return Err(failure);
                            }
                            error!(%cause, attempt = restarts, wait = ?backoff, "Zaino lost its validator; restarting");
                            // The recorder outlives this loop, so nothing else resets:
                            // a crash-looping indexer scrapes like a healthy one
                            #[cfg(feature = "prometheus")]
                            {
                                crate::metrics::record_restart();
                                crate::admin::clear_heartbeat();
                            }
                            tokio::time::sleep(backoff).await;
                            backoff = backoff.saturating_mul(2).min(RESTART_MAX);
                            continue;
                        }
                        Err(e) => {
                            error!(cause = %error_chain(&e), "exiting Zaino with error");
                            return Err(e);
                        }
                    },
                    Err(e) => {
                        error!(cause = %error_chain(&e), "Zaino exited early with error");
                        return Err(e.into());
                    }
                }
            }
            Err(e) => {
                error!(cause = %error_chain(&e), "Zaino failed to start");
                return Err(e);
            }
        }
    }
}
