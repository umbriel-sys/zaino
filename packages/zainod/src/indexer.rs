//! The indexer: the service over the configured validator, and the servers
//! over the service.

use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use tokio::time::Instant;
use tracing::info;
use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle};
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

/// Zaino, the Zingo-Indexer.
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
            .map_err(|error| IndexerError::Validator(error.to_string()))?;
        info!(build = %node.build, subversion = %node.subversion, "connected to the validator");

        let service = Arc::new(Service::spawn(source, config.service_config()).await?);
        wait_for_head(&service).await?;
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
        let task = tokio::task::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(100));
            let mut last_log = Instant::now();
            let shutdown = shutdown_signal();
            tokio::pin!(shutdown);
            loop {
                // every tick: the heartbeat `/livez` answers from
                #[cfg(feature = "prometheus")]
                crate::admin::heartbeat();
                if last_log.elapsed() >= Duration::from_secs(10) {
                    indexer.log_status();
                    last_log = Instant::now();
                }
                if indexer.check_for_critical_errors() {
                    indexer.close().await;
                    return Err(IndexerError::Restart);
                }
                if indexer.check_for_shutdown() {
                    indexer.close().await;
                    return Ok(());
                }
                tokio::select! {
                    _ = tick.tick() => {}
                    // pod teardown is SIGTERM: the same graceful close, so the
                    // store flushes
                    _ = &mut shutdown => {
                        info!("received shutdown signal; closing Zaino gracefully");
                        indexer.close().await;
                        return Ok(());
                    }
                }
            }
        });
        Ok(Running { task, service })
    }

    /// Whether the indexer must restart: the gRPC server is gone, or, while
    /// nothing is closing, a component is not live.
    fn check_for_critical_errors(&self) -> bool {
        let critical = match self.statuses() {
            None => true,
            Some(statuses) => !closing(&statuses) && statuses.iter().any(|s| !s.is_live()),
        };
        if critical {
            tracing::error!(statuses = ?self.statuses(), "check_for_critical_errors triggered");
        }
        critical
    }

    /// Whether a component is closing.
    fn check_for_shutdown(&self) -> bool {
        self.statuses().is_some_and(|statuses| closing(&statuses))
    }

    /// Closes the servers, then stops the service; a primary store flushes.
    async fn close(&mut self) {
        if let Some(mut json_server) = self.json_server.take() {
            json_server.close().await;
            json_server.status.set(Lifecycle::Offline, Health::Offline);
        }
        if let Some(mut server) = self.server.take() {
            server.close().await;
            server.status.set(Lifecycle::Offline, Health::Offline);
        }
        if let Err(error) = self.service.shutdown().await {
            tracing::error!(%error, "the store did not flush cleanly");
        }
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

    /// Logs every component's status, and whether every answer is local yet:
    /// a store still building serves the same reads from the validator.
    pub fn log_status(&self) {
        let service = self.service.statuses();
        info!(
            store = %service.store,
            head = %service.head,
            mempool = %service.mempool,
            coherence = %service.coherence,
            local = self.service.is_local(),
            json_rpc = %self.json_server.as_ref().map_or_else(|| offline("JSON-RPC"), JsonRpcServer::status),
            grpc = %self.server.as_ref().map_or_else(|| offline("gRPC"), TonicServer::status),
            "Zaino status check"
        );
    }
}

/// Waits until the chain head has reached the validator's tip. The store is
/// not waited on: it builds in the background and the validator answers
/// meanwhile.
async fn wait_for_head<S: ServiceSource>(service: &Service<S>) -> Result<(), IndexerError> {
    loop {
        let head = service.statuses().head;
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
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(e) => {
                tracing::warn!(%e, "could not install SIGTERM handler; falling back to ctrl-c only");
                if let Err(error) = tokio::signal::ctrl_c().await {
                    tracing::error!(%error, "could not listen for ctrl-c; shutting down");
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::error!(%error, "could not listen for ctrl-c; shutting down");
        }
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
             Thank you for using ZingoLabs Zaino!

       - Donate to us at https://free2z.cash/zingolabs.

****** Please note Zaino is currently in development and should not be used to run mainnet nodes. ******
    "#;
    println!("{welcome_message}");
}
