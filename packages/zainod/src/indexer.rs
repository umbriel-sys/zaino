//! Zaino : Zingo-Indexer implementation.

use tokio::time::Instant;
use tracing::info;

use zaino_component::{ComponentName, ComponentStatus, Health, Lifecycle};
use zaino_rpc::probe_node;
use zaino_serve::{
    rpc::grpc_routes,
    server::{config::GrpcServerConfig, grpc::TonicServer, jsonrpc::JsonRpcServer},
};
use zaino_state::legacy::status::component_status;
use zaino_state::{
    IndexerService, LightWalletService, NodeBackedIndexerService, NodeBackedIndexerServiceConfig,
    ZcashIndexer, ZcashService,
};

use crate::{config::ZainodConfig, error::IndexerError};

/// Zaino, the Zingo-Indexer.
pub struct Indexer<Service: ZcashService + LightWalletService> {
    /// JsonRPC server.
    ///
    /// Disabled by default.
    json_server: Option<JsonRpcServer>,
    /// GRPC server.
    server: Option<TonicServer>,
    /// Chain fetch service state process handler..
    service: Option<IndexerService<Service>>,
}

/// Starts Indexer service.
///
/// Currently only takes an IndexerConfig.
pub async fn start_indexer(
    config: ZainodConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    startup_message();
    info!("Starting Zaino");
    spawn_indexer(config).await
}

/// Spawns a new Indexer server.
pub async fn spawn_indexer(
    config: ZainodConfig,
) -> Result<tokio::task::JoinHandle<Result<(), IndexerError>>, IndexerError> {
    config.check_config()?;
    info!(
        address = %config.validator_settings.validator_jsonrpc_listen_address,
        "Checking connection with node"
    );
    if let Some(donation_address) = &config.donation_address {
        info!(%donation_address, "instance donation address");
    }
    let zebrad_uri = probe_node(
        &config.validator_settings.validator_jsonrpc_listen_address,
        config.validator_settings.validator_cookie_path.as_deref(),
        config.validator_settings.validator_user.clone(),
        config.validator_settings.validator_password.clone(),
    )
    .await?;

    info!(uri = %zebrad_uri, "Connected to node via JsonRPSee");

    // Both the JSON-RPC (`Rpc`) and direct-`ReadStateService` (`Direct`) connections are
    // now served by the single `NodeBackedIndexerService`; the connection is selected
    // inside the config conversion from `config.backend`.
    let service_config = NodeBackedIndexerServiceConfig::try_from(config.clone())?;
    Indexer::<NodeBackedIndexerService>::launch_inner(service_config, config)
        .await
        .map(|res| res.0)
}

impl<Service: ZcashService + LightWalletService + Send + Sync + 'static> Indexer<Service>
where
    IndexerError: From<<Service::Subscriber as ZcashIndexer>::Error>,
{
    /// Spawns a new Indexer server.
    // TODO: revise whether returning the subscriber here is the best way to access the service after the indexer is spawned.
    pub async fn launch_inner(
        service_config: Service::Config,
        indexer_config: ZainodConfig,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), IndexerError>>,
            Service::Subscriber,
        ),
        IndexerError,
    > {
        Self::launch_inner_impl(service_config, indexer_config, None, None).await
    }

    /// Launches the indexer on pre-bound listeners (test-only).
    ///
    /// The harness binds `127.0.0.1:0` for the gRPC server (and the JSON-RPC
    /// server when enabled), reads the OS-assigned ports, and hands the open
    /// sockets here — eliminating the pick-a-port / bind-later race that
    /// otherwise flakes under parallel test execution. `json_listener` must be
    /// `Some` exactly when `indexer_config.json_server_settings` is `Some`.
    #[cfg(feature = "test_dependencies")]
    pub async fn launch_inner_with_listeners(
        service_config: Service::Config,
        indexer_config: ZainodConfig,
        grpc_listener: std::net::TcpListener,
        json_listener: Option<std::net::TcpListener>,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), IndexerError>>,
            Service::Subscriber,
        ),
        IndexerError,
    > {
        Self::launch_inner_impl(
            service_config,
            indexer_config,
            Some(grpc_listener),
            json_listener,
        )
        .await
    }

    async fn launch_inner_impl(
        service_config: Service::Config,
        indexer_config: ZainodConfig,
        grpc_listener: Option<std::net::TcpListener>,
        json_listener: Option<std::net::TcpListener>,
    ) -> Result<
        (
            tokio::task::JoinHandle<Result<(), IndexerError>>,
            Service::Subscriber,
        ),
        IndexerError,
    > {
        let service = IndexerService::<Service>::spawn(service_config).await?;
        let service_subscriber = service.inner_ref().get_subscriber();

        let json_server = match indexer_config.json_server_settings {
            Some(json_server_config) => Some(match json_listener {
                #[cfg(feature = "test_dependencies")]
                Some(listener) => {
                    JsonRpcServer::spawn_from_listener(
                        service.inner_ref().get_subscriber(),
                        json_server_config,
                        listener,
                    )
                    .await?
                }
                _ => {
                    JsonRpcServer::spawn(service.inner_ref().get_subscriber(), json_server_config)
                        .await?
                }
            }),
            None => None,
        };

        let grpc_config = GrpcServerConfig {
            listen_address: indexer_config.grpc_settings.listen_address,
            tls: indexer_config.grpc_settings.tls,
            limits: indexer_config.grpc_settings.limits,
        };
        let grpc_server = match grpc_listener {
            #[cfg(feature = "test_dependencies")]
            Some(listener) => {
                TonicServer::spawn_from_listener_with_routes(
                    |shutdown| grpc_routes(service.inner_ref().get_subscriber(), shutdown),
                    grpc_config,
                    listener,
                )
                .await?
            }
            _ => {
                TonicServer::spawn_with_routes(
                    |shutdown| grpc_routes(service.inner_ref().get_subscriber(), shutdown),
                    grpc_config,
                )
                .await?
            }
        };

        let mut indexer = Self {
            json_server,
            server: Some(grpc_server),
            service: Some(service),
        };

        let mut server_interval = tokio::time::interval(tokio::time::Duration::from_millis(100));
        let mut last_log_time = Instant::now();
        let log_interval = tokio::time::Duration::from_secs(10);

        let serve_task = tokio::task::spawn(async move {
            let shutdown = shutdown_signal();
            tokio::pin!(shutdown);
            loop {
                // Every tick (100ms): the heartbeat `/livez` answers from
                #[cfg(feature = "prometheus")]
                crate::admin::heartbeat();

                // Log the servers status.
                if last_log_time.elapsed() >= log_interval {
                    indexer.log_status();
                    last_log_time = Instant::now();
                }

                // Check for restart signals.
                if indexer.check_for_critical_errors() {
                    indexer.close().await;
                    return Err(IndexerError::Restart);
                }

                // Check for shutdown signals.
                if indexer.check_for_shutdown() {
                    indexer.close().await;
                    return Ok(());
                }

                tokio::select! {
                    _ = server_interval.tick() => {}
                    // Pod teardown = SIGTERM; same graceful close, so the db and
                    // mempool are not killed mid-write
                    _ = &mut shutdown => {
                        info!("received shutdown signal; closing Zaino gracefully");
                        indexer.close().await;
                        return Ok(());
                    }
                }
            }
        });

        Ok((serve_task, service_subscriber.inner()))
    }

    /// Whether the indexer must restart: the service or the gRPC server is
    /// gone, or, while nothing is closing, a component is not live.
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

    /// Sets the servers to close gracefully.
    async fn close(&mut self) {
        if let Some(mut json_server) = self.json_server.take() {
            json_server.close().await;
            json_server.status.set(Lifecycle::Offline, Health::Offline);
        }

        if let Some(mut server) = self.server.take() {
            server.close().await;
            server.status.set(Lifecycle::Offline, Health::Offline);
        }

        if let Some(service) = self.service.take() {
            let mut service = service.inner();
            service.close();
        }
    }

    /// The status of the indexer service, the gRPC server and, when running,
    /// the JSON-RPC server; `None` while the service or the gRPC server is
    /// not held.
    pub fn statuses(&self) -> Option<Vec<ComponentStatus>> {
        let service = self.service.as_ref()?;
        let grpc = self.server.as_ref()?;
        let mut statuses = vec![
            component_status(SERVICE, service.inner_ref().status()),
            grpc.status(),
        ];
        statuses.extend(self.json_server.as_ref().map(JsonRpcServer::status));
        Some(statuses)
    }

    /// Logs the indexers status.
    pub fn log_status(&self) {
        let service_status = match &self.service {
            Some(service) => component_status(SERVICE, service.inner_ref().status()),
            None => offline(SERVICE),
        };

        // `chain_state: Ready` on its own is ambiguous: while initial sync or a migration runs, an
        // ephemeral passthrough serves finalised-state reads and reports `Ready` exactly like the
        // real on-disk index. Reporting the mode next to the status is what lets an operator — or a
        // containerised test polling this line — tell the two apart.
        let finalised_state_mode = self
            .service
            .as_ref()
            .map(|service| service.inner_ref().finalised_state_mode().to_string())
            .unwrap_or_else(|| "unknown".to_string());

        let json_server_status = match &self.json_server {
            Some(json_server) => json_server.status(),
            None => offline("JSON-RPC"),
        };

        let grpc_server_status = match &self.server {
            Some(server) => server.status(),
            None => offline("gRPC"),
        };

        info!(
            chain_state = %service_status,
            fs_mode = %finalised_state_mode,
            json_rpc = %json_server_status,
            grpc = %grpc_server_status,
            "Zaino status check"
        );
    }
}

/// The name the indexer service reports status under.
const SERVICE: &str = "indexer";

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
        let _ = tokio::signal::ctrl_c().await;
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
