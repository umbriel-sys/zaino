//! The indexer: the service over the configured validator, and the servers
//! over the service.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::time::Instant;
use tracing::info;
use zaino_component::{error_chain, ComponentName, ComponentStatus, Health, Lifecycle};
use zaino_service::{Service, ServiceError, ServiceSource};

use crate::serve::{
    backend::{Backend, BuildInfo, ServeInfo},
    config::GrpcServerConfig,
    grpc::TonicServer,
    jsonrpc::JsonRpcServer,
};
use crate::{config::ZainodConfig, error::IndexerError};

/// How often the chain head is looked at while it reaches the validator's tip.
const HEAD_POLL: Duration = Duration::from_millis(100);

/// How often a chain head still building is reported.
const HEAD_REPORT: Duration = Duration::from_secs(10);

/// Listeners bound ahead of the launch (tests bind `127.0.0.1:0` and hand
/// the open sockets over); `None` binds the configured address.
#[derive(Debug, Default)]
pub struct Listeners {
    /// The gRPC server's.
    pub grpc: Option<std::net::TcpListener>,
    /// The JSON-RPC server's.
    pub jsonrpc: Option<std::net::TcpListener>,
}

/// A running indexer.
pub struct Running {
    /// Resolves when the indexer stops.
    pub task: tokio::task::JoinHandle<Result<(), IndexerError>>,
    /// The service behind the servers.
    pub service: Arc<dyn ServiceHandle>,
}

/// What a caller holding a running indexer may ask of its service.
pub trait ServiceHandle: Send + Sync {
    /// Resolves once nothing is still building: every answer is then local
    /// where an index serves it.
    fn wait_until_local(&self) -> BoxFuture<'_, Result<(), ServiceError>>;

    /// The store's committed tip, `None` while empty or without a store.
    fn store_tip(&self) -> Option<zaino_primitives::types::Height>;

    /// Whether the store has caught up with the chain head's floor; true
    /// without one.
    fn is_caught_up(&self) -> bool;
}

impl<S: ServiceSource> ServiceHandle for Service<S> {
    fn wait_until_local(&self) -> BoxFuture<'_, Result<(), ServiceError>> {
        Box::pin(Service::wait_until_local(self))
    }

    fn store_tip(&self) -> Option<zaino_primitives::types::Height> {
        Service::store_tip(self)
    }

    fn is_caught_up(&self) -> bool {
        Service::is_caught_up(self)
    }
}

/// Zaino, the indexer.
pub struct Indexer<S: ServiceSource> {
    /// JsonRPC server; off unless configured.
    json_server: Option<JsonRpcServer>,
    /// gRPC server.
    server: Option<TonicServer>,
    /// The service both serve from.
    service: Arc<Service<S>>,
}

/// Starts the indexer the config describes.
pub async fn start_indexer(
    config: ZainodConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    startup_message();
    info!("Starting Zaino");
    spawn_indexer(config).await
}

/// Spawns the indexer the config describes.
pub async fn spawn_indexer(
    config: ZainodConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    Ok(launch(config, Listeners::default()).await?.task)
}

/// Launches the indexer the config describes, on `listeners` where given.
pub async fn launch(config: ZainodConfig, listeners: Listeners) -> Result<Running, IndexerError> {
    config.check_config()?;
    if let Some(donation_address) = &config.donation_address {
        info!(%donation_address, "instance donation address");
    }
    crate::source::launch(config, listeners).await
}

impl<S: ServiceSource> Indexer<S> {
    /// Starts the service on `source`, waits for its chain head, and serves.
    pub(crate) async fn launch(
        source: Arc<S>,
        config: ZainodConfig,
        listeners: Listeners,
    ) -> Result<Running, IndexerError> {
        let node = zaino_source::OneShotGetNodeInfo::get_node_info(source.as_ref())
            .await
            .map_err(|error| IndexerError::Validator(error_chain(&error)))?;
        info!(build = %node.build, subversion = %node.subversion, "connected to the validator");
        info!(
            store = %config.persistence.path.display(),
            set = ?config.indexes.set,
            enable = ?config.indexes.enable,
            "starting the store, the chain head and the mempool"
        );
        let starting = crate::source::Heartbeat::start("starting the services");
        let service = Arc::new(Service::spawn(source, config.service_config()).await?);
        drop(starting);
        info!(
            blocks = crate::config::NONFINALISED_DEPTH,
            "building the chain head: the most recent blocks, read from the validator"
        );
        wait_for_head(&service).await?;
        info!("the chain head is at the validator's tip");
        let grpc_address = config.serve.grpc.listen_address;
        let jsonrpc_address = config.serve.jsonrpc.as_ref().map(|j| j.listen_address);
        let backend = Backend {
            service: Arc::clone(&service),
            info: Arc::new(ServeInfo {
                network: config.network,
                build: BuildInfo::current(),
                validator_build: node.build,
                validator_subversion: node.subversion,
                donation_address: config.donation_address.clone(),
                stream_timeout: Duration::from_secs(u64::from(config.serve.stream_timeout_secs)),
                stream_channel_size: usize::try_from(config.serve.stream_channel_size)
                    .unwrap_or(usize::MAX),
            }),
        };

        let json_server = match config.serve.jsonrpc {
            Some(jsonrpc) => {
                let methods = crate::serve::jsonrpc::methods::methods(backend.clone());
                Some(match listeners.jsonrpc {
                    Some(listener) => {
                        JsonRpcServer::spawn_from_listener(methods, jsonrpc, listener).await?
                    }
                    None => JsonRpcServer::spawn(methods, jsonrpc).await?,
                })
            }
            None => None,
        };
        let grpc_config: GrpcServerConfig = config.serve.grpc;
        let routes = |shutdown| crate::serve::grpc::lightwallet::routes(backend.clone(), shutdown);
        let grpc_server = match listeners.grpc {
            Some(listener) => {
                TonicServer::spawn_from_listener_with_routes(routes, grpc_config, listener).await?
            }
            None => TonicServer::spawn_with_routes(routes, grpc_config).await?,
        };

        let mut indexer = Self {
            json_server,
            server: Some(grpc_server),
            service: Arc::clone(&service),
        };
        info!(
            grpc = %grpc_address,
            json_rpc = ?jsonrpc_address,
            local = indexer.service.is_local(),
            "Zaino is serving; the store builds in the background and the validator answers \
             what it does not hold yet (a status line follows every 10 seconds)"
        );
        indexer.log_status();
        let task = tokio::task::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            let mut last_log = Instant::now();
            let shutdown = shutdown_signal();
            tokio::pin!(shutdown);
            loop {
                // every tick: the heartbeat `/livez` answers from
                #[cfg(feature = "prometheus")]
                crate::admin::heartbeat(indexer.readiness());
                if last_log.elapsed() >= Duration::from_secs(10) {
                    indexer.log_status();
                    last_log = Instant::now();
                }
                if let Some(failure) = indexer.check_for_critical_errors() {
                    tracing::error!(%failure, "a component failed; closing Zaino");
                    if let Err(error) = indexer.close().await {
                        tracing::error!(%error, "closing after the failure");
                    }
                    return Err(failure);
                }
                if indexer.check_for_shutdown() {
                    return indexer.close().await;
                }
                tokio::select! {
                    _ = tick.tick() => {}
                    // pod teardown is SIGTERM: the same graceful close, so the
                    // store flushes
                    signal = &mut shutdown => {
                        let closed = indexer.close().await;
                        return match signal {
                            Ok(()) => {
                                info!("received shutdown signal; closed Zaino gracefully");
                                closed
                            }
                            // without its signals Zaino cannot be stopped cleanly
                            Err(error) => Err(IndexerError::Critical(format!(
                                "cannot listen for shutdown signals: {error}"
                            ))),
                        };
                    }
                }
            }
        });
        Ok(Running { task, service })
    }

    /// Why the indexer must stop: the gRPC server or the admin endpoint is
    /// gone, or, while nothing is closing, a component is not live.
    /// [`IndexerError::Restart`] where every failed component failed only
    /// because the validator stopped answering, else
    /// [`IndexerError::Critical`].
    fn check_for_critical_errors(&self) -> Option<IndexerError> {
        #[cfg(feature = "prometheus")]
        if crate::admin::stopped() {
            return Some(IndexerError::Critical(
                "the admin endpoint stopped: no /metrics or probes".to_string(),
            ));
        }
        let Some(statuses) = self.statuses() else {
            return Some(IndexerError::Critical(
                "the gRPC server is gone".to_string(),
            ));
        };
        failure_of(&statuses)
    }

    /// Whether a component is closing.
    fn check_for_shutdown(&self) -> bool {
        self.statuses().is_some_and(|statuses| closing(&statuses))
    }

    /// Closes the servers, then stops the service; a primary store flushes,
    /// and a flush that fails is the error.
    async fn close(&mut self) -> Result<(), IndexerError> {
        if let Some(mut json_server) = self.json_server.take() {
            json_server.close().await;
            json_server.status.set(Lifecycle::Offline, Health::Offline);
        }
        if let Some(mut server) = self.server.take() {
            server.close().await;
            server.status.set(Lifecycle::Offline, Health::Offline);
        }
        self.service
            .shutdown()
            .await
            .map_err(|error| IndexerError::Flush(error_chain(&error)))
    }

    /// The status of every service component, the gRPC server and, when
    /// running, the JSON-RPC server; `None` once the gRPC server is gone.
    pub fn statuses(&self) -> Option<Vec<ComponentStatus>> {
        let grpc = self.server.as_ref()?;
        let mut statuses: Vec<ComponentStatus> = self.service.statuses().all().cloned().collect();
        statuses.push(grpc.status());
        statuses.extend(self.json_server.as_ref().map(JsonRpcServer::status));
        Some(statuses)
    }

    /// What `/readyz` reports: the components requests go through, the
    /// store and its sync, and every index.
    #[cfg(feature = "prometheus")]
    fn readiness(&self) -> crate::admin::Readiness {
        let service = self.service.statuses();
        let server =
            |status: Option<ComponentStatus>, name| status.unwrap_or_else(|| offline(name));
        let mut serving = vec![
            service.head,
            service.mempool,
            service.coherence,
            service.source,
            server(self.server.as_ref().map(TonicServer::status), "gRPC"),
        ];
        serving.extend(self.json_server.as_ref().map(JsonRpcServer::status));
        crate::admin::Readiness {
            serving,
            building: std::iter::once(service.store).chain(service.sync).collect(),
            indexes: self.service.index_states(),
            local: self.service.is_local(),
        }
    }

    /// Logs every component's status, and whether every answer is local yet:
    /// a store still building serves the same reads from the validator.
    pub fn log_status(&self) {
        let service = self.service.statuses();
        info!(
            store = %service.store,
            head = %service.head,
            mempool = %service.mempool,
            coherence = %service.coherence,
            source = %service.source,
            sync = %service.sync.map_or_else(|| "no primary store".to_owned(), |sync| sync.to_string()),
            local = self.service.is_local(),
            json_rpc = %self.json_server.as_ref().map_or_else(|| "not configured".to_owned(), |server| server.status().to_string()),
            grpc = %self.server.as_ref().map_or_else(|| offline("gRPC"), TonicServer::status),
            "Zaino status check"
        );
    }
}

/// Waits until the chain head has reached the validator's tip. The store is
/// not waited on: it builds in the background and the validator answers
/// meanwhile.
async fn wait_for_head<S: ServiceSource>(service: &Service<S>) -> Result<(), IndexerError> {
    let mut reported = Instant::now();
    loop {
        let statuses = service.statuses();
        // the head's own retries are bounded, and so is every other
        // component's: one that fails ends the wait
        if let Some(failed) = statuses.all().find(|status| !status.is_live()) {
            return Err(IndexerError::Critical(format!(
                "a component failed before the chain head was built: {failed}"
            )));
        }
        let head = statuses.head;
        if reported.elapsed() >= HEAD_REPORT {
            info!(%head, "waiting for the chain head");
            reported = Instant::now();
        }
        match (head.lifecycle, head.health) {
            (Lifecycle::Ready | Lifecycle::Closing, _) => return Ok(()),
            (Lifecycle::Offline, _) | (_, Health::Critical | Health::Offline) => {
                return Err(IndexerError::MiscIndexerError(format!(
                    "the chain head did not start: {head}"
                )))
            }
            _ => tokio::time::sleep(HEAD_POLL).await,
        }
    }
}

/// Why the indexer must stop, from its components' statuses: nothing while
/// one is closing or all are live. Each failed component is named with its
/// cause; a restart is asked for only where every one of them failed
/// because the validator stopped answering.
fn failure_of(statuses: &[ComponentStatus]) -> Option<IndexerError> {
    if closing(statuses) {
        return None;
    }
    let failed: Vec<&ComponentStatus> = statuses.iter().filter(|s| !s.is_live()).collect();
    if failed.is_empty() {
        return None;
    }
    let causes = failed
        .iter()
        .map(|status| status.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    Some(match failed.iter().all(|status| status.retryable) {
        true => IndexerError::Restart(causes),
        false => IndexerError::Critical(causes),
    })
}

/// Whether any component is closing.
fn closing(statuses: &[ComponentStatus]) -> bool {
    statuses
        .iter()
        .any(|status| status.lifecycle == Lifecycle::Closing)
}

/// A component that is not running.
fn offline(name: &'static str) -> ComponentStatus {
    ComponentStatus::new(ComponentName(name), Lifecycle::Offline, Health::Offline)
}

/// Resolves on SIGTERM (pod teardown) or ctrl-c; ctrl-c only off unix
async fn shutdown_signal() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            interrupted = tokio::signal::ctrl_c() => interrupted,
            terminated = term.recv() => terminated.ok_or_else(|| {
                std::io::Error::other("the SIGTERM stream ended")
            }),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await
    }
}

/// Prints Zaino's startup message.
fn startup_message() {
    let welcome_message = r#"
       ░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓░▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒████▓▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒░▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒██▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒██▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓███▓██▓▒▒▒▒▒
       ▒▒▒▒▒▒▒▓▓▓▓▒███▓░▒▓▓████████████████▓▓▒▒▒▒▒▒▒
       ▒▒▒▒▒▒▓▓▓▓▒▓████▓▓███████████████████▓▒▓▓▒▒▒▒
       ▒▒▒▒▒▓▓▓▓▓▒▒▓▓▓▓████████████████████▓▒▓▓▓▒▒▒▒
       ▒▒▒▒▒▓▓▓▓▓█████████████████████████▓▒▓▓▓▓▓▒▒▒
       ▒▒▒▒▓▓▓▒▓█████████████████████████▓▓▓▓▓▓▓▓▒▒▒
       ▒▒▒▒▒▓▓▓████████████████████████▓▓▓▓▓▓▓▓▓▒▒▒▒
       ▒▒▒▒▒▓▒███████████████████████▒▓▓▓▓▓▓▓▓▓▓▒▒▒▒
       ▒▒▒▒▒▒▓███████████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒
       ▒▒▒▒▒▒▓███████████████▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒
       ▒▒▒▒▒▒▓██████████▓▓▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒
       ▒▒▒▒███▓▒▓▓▓▓▓▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▓████▒▒▒▒▒▒▒▒▓▓▓▓▓▓▓▓▓▓▓▓▓▓▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
       ▒▒▒▒░▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒▒
                  Thank you for using Zaino!

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{welcome_message}");
}

#[cfg(test)]
mod tests {
    use zaino_chain_store_service::{IndexesConfig, NamedSet};
    use zaino_primitives::types::Network;
    use zaino_proto::proto::service::{
        compact_tx_streamer_client::CompactTxStreamerClient, ChainSpec,
    };
    use zaino_source_mock::{MockChain, NodeFacts};

    use super::*;

    /// A store-less config for `network`, serving gRPC on a loopback port.
    fn config(network: Network) -> (ZainodConfig, Listeners, std::net::SocketAddr) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let address = listener.local_addr().expect("bound");
        let mut config = ZainodConfig {
            network,
            indexes: IndexesConfig {
                set: Some(NamedSet::Empty),
                ..IndexesConfig::default()
            },
            ..ZainodConfig::default()
        };
        config.serve.grpc.listen_address = address;
        let listeners = Listeners {
            grpc: Some(listener),
            jsonrpc: None,
        };
        (config, listeners, address)
    }

    fn validator() -> Arc<MockChain> {
        Arc::new(MockChain::new(
            NodeFacts::regtest(),
            crate::serve::testing::blocks(20),
        ))
    }

    /// The daemon launches on a validator of its configured network and
    /// serves that validator's tip.
    #[tokio::test]
    async fn the_indexer_launches_and_serves() {
        let (config, listeners, address) = config(Network::Regtest);
        let running = Indexer::launch(validator(), config, listeners)
            .await
            .expect("launches");
        running.service.wait_until_local().await.expect("local");
        assert!(running.service.is_caught_up());
        assert_eq!(running.service.store_tip(), None, "no store");
        let mut client = CompactTxStreamerClient::connect(format!("http://{address}"))
            .await
            .expect("connects");
        let tip = client
            .get_latest_block(ChainSpec {})
            .await
            .expect("answered")
            .into_inner();
        assert_eq!(tip.height, 20);
        running.task.abort();
    }

    fn status(name: &'static str, lifecycle: Lifecycle, health: Health) -> ComponentStatus {
        ComponentStatus::new(ComponentName(name), lifecycle, health)
    }

    fn failed(name: &'static str, reason: &str, retryable: bool) -> ComponentStatus {
        let mut status = status(name, Lifecycle::Ready, Health::Healthy);
        assert!(status.fail(reason, retryable));
        status
    }

    /// Live components, degraded ones included, stop nothing; nor does a
    /// failure while the indexer is closing.
    #[test]
    fn live_or_closing_components_stop_nothing() {
        let ready = status("head", Lifecycle::Ready, Health::Healthy);
        let degraded = status("mempool", Lifecycle::Syncing, Health::Recoverable);
        assert!(failure_of(&[ready.clone(), degraded]).is_none());
        let closing = status("store", Lifecycle::Closing, Health::Offline);
        assert!(failure_of(&[ready, closing, failed("head", "gone", false)]).is_none());
    }

    /// A component that failed for a fault of Zaino's own ends the indexer
    /// with its cause; a restart is asked for only where every failure was
    /// the validator not answering.
    #[test]
    fn a_failed_component_is_an_exit_unless_every_failure_is_the_validators() {
        let ready = status("mempool", Lifecycle::Ready, Health::Healthy);
        let lost = failed("head", "gave up on the validator", true);
        let broken = failed("store", "an index is corrupt", false);

        match failure_of(&[ready.clone(), lost.clone()]) {
            Some(IndexerError::Restart(causes)) => {
                assert!(causes.contains("gave up on the validator"), "{causes}");
            }
            other => panic!("a restart was expected, not {other:?}"),
        }
        match failure_of(&[ready, lost, broken]) {
            Some(IndexerError::Critical(causes)) => {
                assert!(causes.contains("an index is corrupt"), "{causes}");
                assert!(causes.contains("gave up on the validator"), "{causes}");
            }
            other => panic!("an exit was expected, not {other:?}"),
        }
    }
}
