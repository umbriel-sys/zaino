//! The validator source, chosen from config: the one place adapters are
//! named. Everything past this module is generic over the service's source
//! bound.

use std::sync::Arc;

use tracing::info;
use zaino_rpc::{auth_from_parts, probe_node, RpcClient, RpcClientConfig};
use zaino_source::{GetBlockchainInfo, OneShotGetChainTip as _, RetryPolicy, ValidatorClient};
use zaino_source_zcash_rpc::ZcashRpcAdapter;
use zaino_source_zebra::{ReadStateSettings, ZebraValidator};

use crate::config::{resolve, ValidatorKind, ZainodConfig};
use crate::error::IndexerError;
use crate::indexer::{Indexer, Listeners, Running};

/// How long between looks at a validator that serves no block yet.
const NOT_READY_RETRY: std::time::Duration = std::time::Duration::from_secs(3);

/// Verifies the validator's activation schedule against the configured
/// network, exactly as the zebra source does when it connects: on Mainnet
/// and the public testnet every reported upgrade must activate where the
/// compiled parameters say, regtest's schedule is the validator's, and a
/// consensus branch this build does not know is refused when in force.
async fn verify_network<V>(
    network: zaino_primitives::types::Network,
    address: &str,
    validator: V,
) -> Result<(), IndexerError>
where
    ValidatorClient<V>: GetBlockchainInfo,
{
    let client = ValidatorClient::new(validator, RetryPolicy::default());
    zaino_source_zebra::adopt_network(network, address, &client)
        .await
        .map(drop)
        .map_err(IndexerError::from)
}

/// Connects to the configured validator and launches the indexer on it.
pub(crate) async fn launch(
    config: ZainodConfig,
    listeners: Listeners,
) -> Result<Running, IndexerError> {
    let validator = &config.validator;
    info!(address = %validator.rpc_address, kind = ?validator.kind, "connecting to the validator");
    let url = probe_node(
        &validator.rpc_address,
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
    )
    .await?;
    let auth = auth_from_parts(
        validator.cookie_path.as_deref(),
        validator.user.clone(),
        validator.password.clone(),
    )?;
    let rpc = move || RpcClientConfig {
        url: url.clone(),
        auth: auth.clone(),
        ..RpcClientConfig::default()
    };

    match (validator.kind, &validator.read_state) {
        (ValidatorKind::Zebra, Some(read_state)) => {
            let settings = ReadStateSettings {
                state_path: read_state.state_path.clone(),
                grpc_address: resolve(
                    &read_state.grpc_address,
                    "validator.read_state.grpc_address",
                )?,
            };
            let source = ZebraValidator::connect(rpc, config.network, settings)
                .await
                .map_err(|error| IndexerError::Validator(error.to_string()))?;
            Indexer::launch(Arc::new(source), config, listeners).await
        }
        (ValidatorKind::ZcashRpc, None) => {
            let client = RpcClient::new(rpc())
                .map_err(|error| IndexerError::Validator(error.to_string()))?;
            let source = ZcashRpcAdapter::new(client);
            // a fresh validator answers RPC before its first block
            while let Err(error) = source.get_chain_tip().await {
                info!(%error, "waiting for the validator to serve its first block");
                tokio::time::sleep(NOT_READY_RETRY).await;
            }
            let adopting = RpcClient::new(rpc())
                .map_err(|error| IndexerError::Validator(error.to_string()))?;
            verify_network(
                config.network,
                &validator.rpc_address,
                ZcashRpcAdapter::new(adopting),
            )
            .await?;
            Indexer::launch(Arc::new(source), config, listeners).await
        }
        (ValidatorKind::Zebra, None) | (ValidatorKind::ZcashRpc, Some(_)) => {
            Err(IndexerError::ConfigError(
                "validator.kind and [validator.read_state] disagree".to_string(),
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use zaino_primitives::types::Network;
    use zaino_source_mock::{MockChain, NodeFacts};

    use super::*;

    /// A generic validator's schedule is verified as the zebra source's is:
    /// a regtest schedule is adopted under `Regtest`, and refused under
    /// Mainnet and the public testnet, whose upgrades activate elsewhere.
    #[tokio::test]
    async fn a_generic_validators_schedule_is_verified() {
        let validator = || MockChain::new(NodeFacts::regtest(), crate::serve::testing::blocks(20));
        verify_network(Network::Regtest, "mock", validator())
            .await
            .expect("regtest adopts the validator's schedule");
        for network in [Network::Mainnet, Network::PubTestnet] {
            let refused = verify_network(network, "mock", validator())
                .await
                .expect_err("another network's schedule");
            assert!(
                matches!(refused, IndexerError::Network(_)),
                "{network:?}: {refused}"
            );
        }
    }
}
