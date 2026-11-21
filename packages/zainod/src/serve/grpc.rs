//! Zaino's gRPC Server Implementation.

use std::time::Duration;

use tokio::sync::watch;
use tonic::{
    service::Routes,
    transport::{server::TcpIncoming, Server},
};
use tracing::warn;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, StatusCell, StatusSource as _,
};

/// The name the gRPC server reports status under.
const NAME: &str = "gRPC";

use crate::serve::{config::GrpcServerConfig, error::ServerError};

pub mod lightwallet;

/// LightWallet gRPC server capable of servicing clients over TCP.
pub struct TonicServer {
    /// Current status of the server.
    pub status: StatusCell,
    /// JoinHandle for the servers `serve` task.
    pub server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    shutdown: watch::Sender<()>,
    /// How long `close` waits for open requests.
    shutdown_timeout: Duration,
}

impl TonicServer {
    /// Starts the gRPC service.
    ///
    /// `routes` is a pre-assembled tonic service router; production
    /// callers build one from an indexer subscriber via
    /// [`lightwallet::routes`]. Decoupling the dispatcher from the
    /// transport layer keeps this function focused on bind / TLS /
    /// shutdown and lets the bind-race regression test
    /// pass [`Routes::default`] instead of a full
    /// trait-stubbed indexer.
    pub async fn spawn(
        routes: Routes,
        server_config: GrpcServerConfig,
    ) -> Result<Self, ServerError> {
        Self::spawn_with_routes(|_| routes, server_config).await
    }

    /// Starts the gRPC service and gives its routes the server shutdown signal.
    pub async fn spawn_with_routes<BuildRoutes>(
        build_routes: BuildRoutes,
        server_config: GrpcServerConfig,
    ) -> Result<Self, ServerError>
    where
        BuildRoutes: FnOnce(watch::Receiver<()>) -> Routes,
    {
        // Bind synchronously so EADDRINUSE / EACCES propagate to the caller
        // instead of being swallowed inside the spawned serve task.
        let tcp_incoming = TcpIncoming::bind(server_config.listen_address)
            .map_err(|e| ServerError::ServerConfigError(format!("gRPC bind failed: {e}")))?;
        Self::spawn_inner(build_routes, server_config, tcp_incoming).await
    }

    /// Starts the gRPC service on a pre-bound listener.
    ///
    /// Lets a test harness bind `127.0.0.1:0`, read the OS-assigned port, and
    /// hand the still-open socket here — closing the pick-a-port / bind-later
    /// race. `TcpIncoming::from` applies the same nodelay/keepalive defaults as
    /// `TcpIncoming::bind`, so the served socket is identical to the production
    /// path.
    pub async fn spawn_from_listener(
        routes: Routes,
        server_config: GrpcServerConfig,
        listener: std::net::TcpListener,
    ) -> Result<Self, ServerError> {
        Self::spawn_from_listener_with_routes(|_| routes, server_config, listener).await
    }

    /// Starts the gRPC service on a pre-bound listener and shares its shutdown signal.
    pub async fn spawn_from_listener_with_routes<BuildRoutes>(
        build_routes: BuildRoutes,
        server_config: GrpcServerConfig,
        listener: std::net::TcpListener,
    ) -> Result<Self, ServerError>
    where
        BuildRoutes: FnOnce(watch::Receiver<()>) -> Routes,
    {
        listener.set_nonblocking(true).map_err(|e| {
            ServerError::ServerConfigError(format!("gRPC listener set_nonblocking failed: {e}"))
        })?;
        let tcp_incoming =
            TcpIncoming::from(tokio::net::TcpListener::from_std(listener).map_err(|e| {
                ServerError::ServerConfigError(format!("gRPC from_std failed: {e}"))
            })?);
        Self::spawn_inner(build_routes, server_config, tcp_incoming).await
    }

    async fn spawn_inner<BuildRoutes>(
        build_routes: BuildRoutes,
        server_config: GrpcServerConfig,
        tcp_incoming: TcpIncoming,
    ) -> Result<Self, ServerError>
    where
        BuildRoutes: FnOnce(watch::Receiver<()>) -> Routes,
    {
        let status = StatusCell::new(ComponentName(NAME), Lifecycle::Spawning, Health::Healthy);
        let (shutdown, mut shutdown_signal) = watch::channel(());
        let routes = build_routes(shutdown.subscribe());

        let limits = server_config.limits;
        let mut server_builder = Server::builder()
            .concurrency_limit_per_connection(limits.concurrency_limit_per_connection.get())
            .max_concurrent_streams(Some(limits.max_concurrent_streams.get()))
            .http2_keepalive_interval(Some(Duration::from_secs(
                limits.keepalive_interval_secs.get(),
            )))
            .http2_keepalive_timeout(Some(Duration::from_secs(
                limits.keepalive_timeout_secs.get(),
            )));
        if let Some(tls_config) = server_config.get_valid_tls().await? {
            // Building the TLS acceptor requires a process-level rustls
            // CryptoProvider.
            zaino_rpc::ensure_default_crypto_provider();
            server_builder = server_builder.tls_config(tls_config).map_err(|e| {
                ServerError::ServerConfigError(format!("TLS configuration error: {e}"))
            })?;
        }

        // a change, or the sender gone: either way the server stops
        let shutdown_signal = async move {
            if shutdown_signal.changed().await.is_err() {
                // the owner is gone: stop as well
            }
        };
        let server_future = server_builder
            .add_routes(routes)
            .serve_with_incoming_shutdown(tcp_incoming, shutdown_signal);

        let task_status = status.clone();
        let server_handle = tokio::task::spawn(async move {
            task_status.set(Lifecycle::Ready, Health::Healthy);
            server_future.await?;
            task_status.set(Lifecycle::Offline, Health::Offline);
            Ok(())
        });

        Ok(TonicServer {
            status,
            server_handle: Some(server_handle),
            shutdown,
            shutdown_timeout: Duration::from_secs(limits.shutdown_timeout_secs.get()),
        })
    }

    /// Closes the server gracefully: its streams end, and open requests get
    /// the configured shutdown timeout before the server is stopped anyway.
    pub async fn close(&mut self) {
        self.status.set(Lifecycle::Closing, Health::Healthy);
        self.shutdown.send_replace(());

        if let Some(mut handle) = self.server_handle.take() {
            match tokio::time::timeout(self.shutdown_timeout, &mut handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => warn!(%error, "gRPC server ended with an error"),
                Ok(Err(error)) => warn!(%error, "gRPC server task did not finish"),
                Err(_) => {
                    handle.abort();
                    warn!("gRPC server stopped with requests still open");
                    self.status.set(Lifecycle::Offline, Health::Offline);
                }
            }
        }
    }

    /// Returns the servers current status.
    ///
    /// If the spawned serve task has finished (panic, tonic-internal
    /// error, etc.), reports `Offline` regardless of the cached status —
    /// otherwise a serve task that died after reporting `Ready` would
    /// keep the indexer's critical-error check from firing.
    pub fn status(&self) -> ComponentStatus {
        if self.server_handle.as_ref().is_some_and(|h| h.is_finished()) {
            return ComponentStatus::new(ComponentName(NAME), Lifecycle::Offline, Health::Offline);
        }
        self.status.status()
    }
}

impl Drop for TonicServer {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
            warn!("Warning: TonicServer dropped without explicit shutdown. Aborting server task.");
        }
    }
}

#[cfg(test)]
mod tests;
