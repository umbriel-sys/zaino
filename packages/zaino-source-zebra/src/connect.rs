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
use crate::secondary::Opening;

/// How long between looks at a validator that is not ready yet.
const NOT_READY_RETRY: Duration = Duration::from_secs(3);
/// How long between looks at a read-state syncer catching up.
const SYNCING_RETRY: Duration = Duration::from_secs(1);

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
        while let Err(_error) = rpc_adapter.get_chain_tip().await {
            #[cfg(feature = "tracing")]
            tracing::info!(error = %_error, "waiting for the validator to serve its first block");
            tokio::time::sleep(NOT_READY_RETRY).await;
        }

        let network = adopt_network(
            network,
            &validator,
            &ValidatorClient::new(adapter()?, RetryPolicy::default()),
        )
        .await?;
        let config = zebra_state::Config {
            cache_dir: read_state.state_path,
            ..zebra_state::Config::default()
        };
        // zebra leaves its secondary state's temp directory behind: tracked
        // across the launch and removed with the adapter
        let opening = tokio::task::spawn_blocking(Opening::begin)
            .await
            .map_err(|error| ConnectError::ReadState(error.to_string()))?
            .map_err(|error| ConnectError::ReadState(error.to_string()))?;
        let launched =
            zebra_rpc::sync::init_read_state_with_syncer(config, &network, read_state.grpc_address)
                .await;
        let secondary = opening
            .finish()
            .map_err(|error| ConnectError::ReadState(error.to_string()))?;
        let (mut state, _latest_chain_tip, _chain_tip_change, syncer) = launched
            .map_err(|error| ConnectError::ReadState(error.to_string()))?
            .map_err(|error| ConnectError::ReadState(error.to_string()))?;

        // hash as well as height: one height names different blocks in a reorg
        loop {
            let (validator_hash, validator_height) = rpc_adapter
                .get_chain_tip()
                .await
                .map_err(|error| ConnectError::Rpc(error.to_string()))?;
            let response = state
                .ready()
                .await
                .map_err(|error| ConnectError::ReadState(error.to_string()))?
                .call(ReadRequest::Tip)
                .await
                .map_err(|error| ConnectError::ReadState(error.to_string()))?;
            let ReadResponse::Tip(tip) = response else {
                return Err(ConnectError::ReadState(
                    "unexpected response to a tip request".to_owned(),
                ));
            };
            let synced = tip.is_some_and(|(height, hash)| {
                u32::from(validator_height) == height.0
                    && <[u8; 32]>::from(validator_hash) == hash.0
            });
            if synced {
                break;
            }
            #[cfg(feature = "tracing")]
            tracing::info!(
                validator_height = u32::from(validator_height),
                "zebra's state is syncing"
            );
            tokio::time::sleep(SYNCING_RETRY).await;
        }

        Ok(Self::with_read_state(
            rpc_adapter,
            ZebraReadStateAdapter::from_service(state, &network, Some(Arc::new(syncer)), secondary),
        ))
    }
}
