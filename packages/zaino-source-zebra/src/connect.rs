//! Connecting to a zebra validator from plain settings.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tower::{Service as _, ServiceExt as _};
use zaino_rpc::{RpcClient, RpcClientConfig};
use zaino_source::{OneShotGetChainTip as _, RetryPolicy, ValidatorClient};
use zebra_state::{ReadRequest, ReadResponse};

use crate::network::adopt_network;
use crate::read_state::ZebraReadStateAdapter;
use crate::routing::ZebraValidator;

/// How long between looks at a read-state syncer catching up.
const SYNCING_RETRY: Duration = Duration::from_secs(1);
/// How long the state may sit behind the validator without advancing before
/// the connection fails.
const SYNCING_STALL: Duration = Duration::from_secs(300);
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
        #[cfg(feature = "tracing")]
        tracing::info!(
            state_path = %read_state.state_path.display(),
            grpc_address = %read_state.grpc_address,
            "opening zebra's state read-only: replaying zebra's write-ahead log can take a minute or more"
        );
        #[cfg(feature = "tracing")]
        let opening = std::time::Instant::now();
        let config = zebra_state::Config {
            cache_dir: read_state.state_path,
            ..zebra_state::Config::default()
        };
        let (mut state, _latest_chain_tip, _chain_tip_change, syncer) =
            zebra_rpc::sync::init_read_state_with_syncer(config, &network, read_state.grpc_address)
                .await
                .map_err(|error| ConnectError::ReadState(error.to_string()))?
                .map_err(|error| ConnectError::ReadState(error.to_string()))?;
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
            // nothing moves the state once the syncer has stopped
            if syncer.is_finished() {
                return Err(ConnectError::ReadState(
                    crate::read_state::SYNCER_STOPPED.to_owned(),
                ));
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

        Ok(Self::with_read_state(
            rpc_adapter,
            ZebraReadStateAdapter::from_service(state, &network, Some(Arc::new(syncer))),
        ))
    }
}
