//! Connecting to a zebra validator from plain settings.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tower::{Service as _, ServiceExt as _};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{OneShotGetChainTip as _, RetryPolicy, ValidatorClient};
use zebra_state::{ReadRequest, ReadResponse};

use crate::network::adopt_network;
use crate::read_state::{Syncer, ZebraReadStateAdapter};
use crate::routing::ZebraValidator;

/// How long between looks at a read-state syncer catching up.
const SYNCING_RETRY: Duration = Duration::from_secs(1);
/// How long the state may sit behind the validator without advancing before
/// the connection fails.
const SYNCING_STALL: Duration = Duration::from_secs(300);
/// How long a syncer that keeps ending before the state is synced is started
/// again, from the first time it ended.
const SYNCER_RETRY_FOR: Duration = Duration::from_secs(300);
/// The wait before a syncer that ended is started again: doubling from the
/// first to the most.
const SYNCER_RETRY_FIRST: Duration = Duration::from_secs(5);
const SYNCER_RETRY_MAX: Duration = Duration::from_secs(60);
/// How often a read-state syncer still catching up is reported.
#[cfg(feature = "tracing")]
const SYNCING_REPORT: Duration = Duration::from_secs(10);

/// Where zebra's state is read directly.
#[derive(Debug, Clone)]
pub struct ReadStateSettings {
    /// zebra's state cache directory.
    pub state_path: PathBuf,
    /// zebra's indexer gRPC address, which feeds the non-finalised blocks.
    pub grpc_address: SocketAddr,
}

/// A zebra validator could not be connected to.
#[derive(Debug, thiserror::Error)]
pub enum ConnectError {
    /// The JSON-RPC client could not be built.
    #[error("the validator's RPC client: {0}")]
    Rpc(String),
    /// The validator's schedule could not be adopted.
    #[error(transparent)]
    Network(#[from] crate::network::AdoptError),
    /// zebra's state could not be read.
    #[error("zebra's state: {0}")]
    ReadState(String),
    /// zebra's state syncer ended before the state reached the validator's
    /// tip. `connect` starts it again; this is returned only by one attempt.
    #[error(
        "zebra's state syncer ended before the state was synced ({cause}); it reads zebra's \
         indexer gRPC at {grpc_address}"
    )]
    SyncerEnded {
        /// Why it ended: its panic message, where it panicked.
        cause: String,
        /// zebra's indexer gRPC address, which the syncer reads.
        grpc_address: SocketAddr,
    },
    /// zebra's state syncer was started again each time it ended, and kept
    /// ending.
    #[error(
        "zebra's state syncer ended {ended} times in {secs}s and was started again each time; \
         the last time: {cause}. Zaino cannot follow the validator without it. Check that \
         zebra's indexer gRPC address ({grpc_address}) is the one zebrad listens on \
         (`indexer_listen_addr` in zebrad's [rpc] config), that zebrad is running, and \
         zebrad's log at that time"
    )]
    SyncerKeepsEnding {
        /// How many times it ended.
        ended: u32,
        /// Seconds from the first time it ended to the last.
        secs: u64,
        /// Why it ended the last time.
        cause: String,
        /// zebra's indexer gRPC address, which the syncer reads.
        grpc_address: SocketAddr,
    },
}

impl ZebraValidator {
    /// Connects to a running zebra: over JSON-RPC and by reading its state
    /// directory.
    ///
    /// Waits until zebra serves a tip (a fresh node answers RPC before its
    /// first block) and until the state syncer holds that tip. The network
    /// is adopted from zebra's own schedule.
    pub async fn connect(
        rpc: impl Fn() -> RpcClientConfig,
        network: zaino_primitives::types::Network,
        read_state: ReadStateSettings,
    ) -> Result<Self, ConnectError> {
        let adapter = || {
            RpcClient::new(rpc())
                .map(crate::rpc_adapter)
                .map_err(|error| ConnectError::Rpc(error.to_string()))
        };
        let validator = rpc().url;
        let rpc_adapter = adapter()?;
        #[cfg(feature = "tracing")]
        tracing::info!(%validator, "asking the validator for its chain tip");
        zaino_source::first_chain_tip(&rpc_adapter, |_error| {
            #[cfg(feature = "tracing")]
            tracing::info!(error = %_error, "waiting for the validator to serve its first block");
        })
        .await
        .map_err(|error| {
            ConnectError::Rpc(format!("the validator serves no chain tip: {error}"))
        })?;

        #[cfg(feature = "tracing")]
        tracing::info!("checking the validator's network and activation heights");
        let network = adopt_network(
            network,
            &validator,
            &ValidatorClient::new(adapter()?, RetryPolicy::default()),
        )
        .await?;
        let (state, syncer) =
            with_syncer_retries(|| Self::open_synced(&rpc_adapter, &read_state, &network)).await?;
        Ok(Self::with_read_state(
            rpc_adapter,
            ZebraReadStateAdapter::with_syncer(state, &network, Some(syncer)),
        ))
    }

    /// Opens zebra's state with its syncer and waits until the state holds
    /// the validator's tip. Fails with [`ConnectError::SyncerEnded`] if the
    /// syncer ends first; whatever fails, the syncer is stopped.
    async fn open_synced(
        rpc_adapter: &zaino_source_zcash_rpc::ZcashRpcAdapter,
        read_state: &ReadStateSettings,
        network: &zebra_chain::parameters::Network,
    ) -> Result<(zebra_state::ReadStateService, Syncer), ConnectError> {
        #[cfg(feature = "tracing")]
        tracing::info!(
            state_path = %read_state.state_path.display(),
            grpc_address = %read_state.grpc_address,
            "opening zebra's state read-only: replaying zebra's write-ahead log can take a minute or more"
        );
        #[cfg(feature = "tracing")]
        let opening = std::time::Instant::now();
        let config = zebra_state::Config {
            cache_dir: read_state.state_path.clone(),
            ..zebra_state::Config::default()
        };
        let (mut state, _latest_chain_tip, _chain_tip_change, syncer) =
            zebra_rpc::sync::init_read_state_with_syncer(config, network, read_state.grpc_address)
                .await
                .map_err(|error| ConnectError::ReadState(error.to_string()))?
                .map_err(|error| ConnectError::ReadState(error.to_string()))?;
        let syncer = Syncer::watch(syncer);
        #[cfg(feature = "tracing")]
        tracing::info!(
            elapsed_secs = opening.elapsed().as_secs(),
            "zebra's state is open; waiting for it to reach the validator's tip"
        );

        // hash as well as height: one height names different blocks in a reorg
        #[cfg(feature = "tracing")]
        let mut reported: Option<std::time::Instant> = None;
        // the state's tip, and when it last moved
        let mut progress: (Option<zebra_chain::block::Height>, std::time::Instant) =
            (None, std::time::Instant::now());
        loop {
            // nothing moves the state once the syncer has ended
            if let Some(cause) = syncer.ended() {
                return Err(ConnectError::SyncerEnded {
                    cause: cause.to_owned(),
                    grpc_address: read_state.grpc_address,
                });
            }
            let (validator_hash, validator_height) = rpc_adapter
                .get_chain_tip()
                .await
                .map_err(|error| ConnectError::Rpc(error.to_string()))?;
            let response = tokio::time::timeout(crate::read_state::READ_TIMEOUT, async {
                state
                    .ready()
                    .await
                    .map_err(|error| ConnectError::ReadState(error.to_string()))?
                    .call(ReadRequest::Tip)
                    .await
                    .map_err(|error| ConnectError::ReadState(error.to_string()))
            })
            .await
            .map_err(|_| ConnectError::ReadState("a tip request timed out".to_owned()))??;
            let ReadResponse::Tip(tip) = response else {
                return Err(ConnectError::ReadState(
                    "unexpected response to a tip request".to_owned(),
                ));
            };
            let state_height = tip.map(|(height, _)| height);
            if state_height != progress.0 {
                progress = (state_height, std::time::Instant::now());
            } else if progress.1.elapsed() >= SYNCING_STALL {
                return Err(ConnectError::ReadState(format!(
                    "the state has sat at {:?} for {}s while the validator is at {}: its syncer \
                     is not advancing it (is zebra's indexer gRPC address reachable?)",
                    state_height.map(|height| height.0),
                    SYNCING_STALL.as_secs(),
                    u32::from(validator_height),
                )));
            }
            let synced = tip.is_some_and(|(height, hash)| {
                u32::from(validator_height) == height.0
                    && <[u8; 32]>::from(validator_hash) == hash.0
            });
            if synced {
                #[cfg(feature = "tracing")]
                tracing::info!(
                    height = u32::from(validator_height),
                    "zebra's state is at the validator's tip"
                );
                break;
            }
            #[cfg(feature = "tracing")]
            if reported.is_none_or(|at| at.elapsed() >= SYNCING_REPORT) {
                tracing::info!(
                    validator_height = u32::from(validator_height),
                    state_height = ?tip.map(|(height, _)| height.0),
                    "zebra's state is behind the validator's tip; waiting"
                );
                reported = Some(std::time::Instant::now());
            }
            tokio::time::sleep(SYNCING_RETRY).await;
        }

        Ok((state, syncer))
    }
}

/// Runs `open`, and runs it again each time zebra's state syncer ends
/// before the state is synced: a new syncer starts from zebra's current
/// chain. Gives up, with the last cause and how often it ended, once
/// [`SYNCER_RETRY_FOR`] has passed since it first ended. Any other failure
/// is returned as it is.
async fn with_syncer_retries<T, F, Fut>(mut open: F) -> Result<T, ConnectError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, ConnectError>>,
{
    let mut first_ended: Option<tokio::time::Instant> = None;
    let mut ended = 0u32;
    let mut wait = SYNCER_RETRY_FIRST;
    loop {
        let (cause, grpc_address) = match open().await {
            Err(ConnectError::SyncerEnded {
                cause,
                grpc_address,
            }) => (cause, grpc_address),
            other => return other,
        };
        ended = ended.saturating_add(1);
        let since = *first_ended.get_or_insert_with(tokio::time::Instant::now);
        if since.elapsed() >= SYNCER_RETRY_FOR {
            return Err(ConnectError::SyncerKeepsEnding {
                ended,
                secs: since.elapsed().as_secs(),
                cause,
                grpc_address,
            });
        }
        #[cfg(feature = "tracing")]
        tracing::warn!(
            %cause,
            %grpc_address,
            ended,
            wait_secs = wait.as_secs(),
            "zebra's state syncer ended before the state was synced; starting it again"
        );
        tokio::time::sleep(wait).await;
        wait = wait.saturating_mul(2).min(SYNCER_RETRY_MAX);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn syncer_ended() -> ConnectError {
        ConnectError::SyncerEnded {
            cause: "it panicked: no block at 7".to_owned(),
            grpc_address: SocketAddr::from(([127, 0, 0, 1], 8230)),
        }
    }

    /// A syncer that ends before the state is synced is started again, and
    /// the connection is made once one holds.
    #[tokio::test(start_paused = true)]
    async fn a_syncer_that_ends_is_started_again() {
        let opened = Cell::new(0u32);
        let connected = with_syncer_retries(|| {
            opened.set(opened.get() + 1);
            let attempt = opened.get();
            async move {
                match attempt {
                    1 | 2 => Err(syncer_ended()),
                    _ => Ok(attempt),
                }
            }
        })
        .await
        .expect("connects");
        assert_eq!(connected, 3);
    }

    /// A syncer that keeps ending is given up on after five minutes, with
    /// how often it ended, why, and what to check.
    #[tokio::test(start_paused = true)]
    async fn a_syncer_that_keeps_ending_fails_the_connection_with_its_cause() {
        let started = tokio::time::Instant::now();
        let opened = Cell::new(0u32);
        let error = with_syncer_retries(|| {
            opened.set(opened.get() + 1);
            async { Err::<(), _>(syncer_ended()) }
        })
        .await
        .expect_err("gives up");
        assert!(started.elapsed() >= SYNCER_RETRY_FOR);
        assert!(started.elapsed() < SYNCER_RETRY_FOR + SYNCER_RETRY_MAX);
        // waits of 5, 10, 20 and 40 seconds, then four of 60: 315 seconds
        assert_eq!(opened.get(), 9);
        assert_eq!(
            error.to_string(),
            "zebra's state syncer ended 9 times in 315s and was started again each time; the \
             last time: it panicked: no block at 7. Zaino cannot follow the validator without \
             it. Check that zebra's indexer gRPC address (127.0.0.1:8230) is the one zebrad \
             listens on (`indexer_listen_addr` in zebrad's [rpc] config), that zebrad is \
             running, and zebrad's log at that time"
        );
    }

    /// Any other failure is not retried.
    #[tokio::test(start_paused = true)]
    async fn another_failure_is_returned_at_once() {
        let opened = Cell::new(0u32);
        let error = with_syncer_retries(|| {
            opened.set(opened.get() + 1);
            async {
                Err::<(), _>(ConnectError::ReadState(
                    "a tip request timed out".to_owned(),
                ))
            }
        })
        .await
        .expect_err("fails");
        assert_eq!(opened.get(), 1);
        assert_eq!(error.to_string(), "zebra's state: a tip request timed out");
    }
}
