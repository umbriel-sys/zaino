//! Zaino's JsonRPC Server Implementation.

use crate::serve::{config::JsonRpcServerConfig, error::ServerError};

mod compat;
mod cookie;
mod deadline;
pub mod methods;
pub(crate) mod metrics;
pub mod wire;

use compat::{HttpCompatLayer, ZcashdErrorCodes};
use cookie::Cookie;
use zaino_component::{
    ComponentName, ComponentStatus, Health, Lifecycle, StatusCell, StatusSource as _,
    StatusWatch as _,
};

use zaino_consensus::MAX_BLOCK_BYTES;

use jsonrpsee::server::{RpcServiceBuilder, ServerBuilder};
use jsonrpsee::Methods;
use std::path::PathBuf;
use tracing::warn;

/// How long open requests get at shutdown before the server is stopped.
const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// JSON-RPC server capable of servicing clients over TCP.
pub struct JsonRpcServer {
    /// Current status of the server.
    pub status: StatusCell,
    /// JoinHandle for the servers `serve` task.
    pub server_handle: Option<tokio::task::JoinHandle<Result<(), ServerError>>>,
    /// Cookie dir.
    cookie_dir: Option<PathBuf>,
}

impl JsonRpcServer {
    /// Starts the JSON-RPC server answering `methods`.
    pub async fn spawn(
        methods: Methods,
        server_config: JsonRpcServerConfig,
    ) -> Result<Self, ServerError> {
        Self::spawn_inner(methods, server_config, None).await
    }

    /// Starts the JSON-RPC server on a pre-bound listener, so a caller can
    /// bind `127.0.0.1:0` and hand the open socket over race-free.
    pub async fn spawn_from_listener(
        methods: Methods,
        server_config: JsonRpcServerConfig,
        listener: std::net::TcpListener,
    ) -> Result<Self, ServerError> {
        Self::spawn_inner(methods, server_config, Some(listener)).await
    }

    async fn spawn_inner(
        methods: Methods,
        server_config: JsonRpcServerConfig,
        listener: Option<std::net::TcpListener>,
    ) -> Result<Self, ServerError> {
        let status = StatusCell::new(
            ComponentName("JSON-RPC"),
            Lifecycle::Spawning,
            Health::Healthy,
        );

        // cookie authentication, as zebrad's, when a directory is configured
        let cookie_dir = server_config.cookie_dir;
        let cookie = match &cookie_dir {
            Some(dir) => {
                let cookie = Cookie::generate();
                cookie.write_to(dir).map_err(|e| {
                    ServerError::ServerConfigError(format!("Failed to write cookie: {e}"))
                })?;
                Some(cookie)
            }
            None => None,
        };

        let max_request_body_size = usize::try_from(MAX_BLOCK_BYTES)
            .unwrap_or(usize::MAX)
            .saturating_mul(2)
            .saturating_add(1024);
        let http_middleware_layer = HttpCompatLayer::new(cookie, max_request_body_size);

        // Before the middleware stack: the metrics layer interns call labels
        // against this method set, so a caller cannot mint a series by inventing
        // a name (see `jsonrpc_metrics`)
        let rpc_module = methods;
        let method_names: crate::serve::jsonrpc::metrics::MethodNames =
            std::sync::Arc::new(rpc_module.method_names().collect());

        // Layer order is load-bearing: first added = outermost, so metrics must sit
        // outside the zcashd compatibility fix to see the code the client receives
        let rpc_middleware = RpcServiceBuilder::new().rpc_logger(1024);
        let rpc_middleware = rpc_middleware.layer_fn(move |service| {
            crate::serve::jsonrpc::metrics::MetricsMiddleware::new(service, method_names.clone())
        });
        let rpc_middleware = rpc_middleware.layer_fn(ZcashdErrorCodes::new);
        // innermost: the timed-out answer is counted and coded like any other
        let rpc_middleware = rpc_middleware.layer_fn(|service| {
            deadline::Deadline::new(service, crate::serve::config::REQUEST_TIMEOUT)
        });

        // Build the JSON-RPC server with middleware integrated. A pre-bound
        // listener (test path) is served directly; otherwise we bind the
        // configured address.
        let builder = ServerBuilder::default()
            .set_http_middleware(tower::ServiceBuilder::new().layer(http_middleware_layer))
            .set_rpc_middleware(rpc_middleware);
        let server = match listener {
            Some(listener) => {
                listener.set_nonblocking(true).map_err(|e| {
                    ServerError::ServerConfigError(format!(
                        "JSON-RPC listener set_nonblocking failed: {e}"
                    ))
                })?;
                builder.build_from_tcp(listener).map_err(|e| {
                    ServerError::ServerConfigError(format!("JSON-RPC server build error: {e}"))
                })?
            }
            None => builder
                .build(server_config.listen_address)
                .await
                .map_err(|e| {
                    ServerError::ServerConfigError(format!("JSON-RPC server build error: {e}"))
                })?,
        };

        let server_handle = server.start(rpc_module);

        let shutdown_signal = shutdown_signal(status.clone());

        let task_status = status.clone();
        let server_task_handle = tokio::task::spawn({
            let server_handle_clone = server_handle.clone();
            async move {
                task_status.set(Lifecycle::Ready, Health::Healthy);

                tokio::select! {
                    _ = shutdown_signal => {
                        if server_handle_clone.stop().is_err() {
                            // already stopped
                        }
                    }
                    _ = server_handle.stopped() => {},
                }

                task_status.set(Lifecycle::Offline, Health::Offline);
                Ok(())
            }
        });

        Ok(JsonRpcServer {
            status,
            server_handle: Some(server_task_handle),
            cookie_dir,
        })
    }

    /// Sets the servers to close gracefully.
    pub async fn close(&mut self) {
        self.status.set(Lifecycle::Closing, Health::Healthy);

        if let Some(dir) = &self.cookie_dir {
            if let Err(e) = cookie::remove_from(dir) {
                warn!(%e, "error removing cookie");
            }
        }

        if let Some(mut handle) = self.server_handle.take() {
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut handle).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => warn!(%error, "JSON-RPC server ended with an error"),
                Ok(Err(error)) => warn!(%error, "JSON-RPC server task did not finish"),
                Err(_) => {
                    handle.abort();
                    warn!("JSON-RPC server stopped with requests still open");
                    self.status.set(Lifecycle::Offline, Health::Offline);
                }
            }
        }
    }

    /// Returns the servers current status: `Offline` once its task has
    /// ended, however it ended.
    pub fn status(&self) -> ComponentStatus {
        if self.server_handle.as_ref().is_some_and(|h| h.is_finished()) {
            return ComponentStatus::new(
                ComponentName("JSON-RPC"),
                Lifecycle::Offline,
                Health::Offline,
            );
        }
        self.status.status()
    }
}

impl Drop for JsonRpcServer {
    fn drop(&mut self) {
        if let Some(handle) = self.server_handle.take() {
            handle.abort();
            warn!(
                "Warning: JsonRpcServer dropped without explicit shutdown. Aborting server task."
            );
        }
    }
}

/// Resolves once the server is closing (or its status is gone).
async fn shutdown_signal(status: StatusCell) {
    let mut watcher = status.subscribe();
    if watcher
        .wait_for(|status| status.lifecycle == Lifecycle::Closing)
        .await
        .is_err()
    {
        // the status is gone: stop as well
    }
}
