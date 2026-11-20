//! Server configuration data.

use std::{
    net::SocketAddr,
    num::{NonZeroU32, NonZeroU64, NonZeroUsize},
    path::PathBuf,
};

use tonic::transport::{Identity, ServerTlsConfig};

use super::error::ServerError;

/// Settings for a Zaino configured with gRPC TLS: paths to key and certificate.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GrpcTls {
    /// Path to the TLS certificate file in PEM format.
    pub cert_path: PathBuf,
    /// Path to the TLS private key file in PEM format.
    pub key_path: PathBuf,
}

/// Configuration data for Zaino's gRPC server.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct GrpcServerConfig {
    /// gRPC server bind addr.
    #[serde(alias = "grpc_listen_address")]
    pub listen_address: SocketAddr,
    /// Enables TLS.
    pub tls: Option<GrpcTls>,
    /// Bounds on what one connection may hold open.
    #[serde(default)]
    pub limits: GrpcLimits,
}

/// What one client connection may hold open (AP-02): a request past a
/// bound queues rather than allocates, and a dead peer is dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct GrpcLimits {
    /// Requests served at once per connection; the rest wait.
    pub concurrency_limit_per_connection: NonZeroUsize,
    /// HTTP/2 streams open at once per connection.
    pub max_concurrent_streams: NonZeroU32,
    /// How often an idle connection is pinged.
    pub keepalive_interval_secs: NonZeroU64,
    /// How long a ping may go unanswered before the connection closes.
    pub keepalive_timeout_secs: NonZeroU64,
}

impl Default for GrpcLimits {
    fn default() -> Self {
        Self {
            concurrency_limit_per_connection: NonZeroUsize::new(32).unwrap_or(NonZeroUsize::MIN),
            max_concurrent_streams: NonZeroU32::new(64).unwrap_or(NonZeroU32::MIN),
            keepalive_interval_secs: NonZeroU64::new(60).unwrap_or(NonZeroU64::MIN),
            keepalive_timeout_secs: NonZeroU64::new(20).unwrap_or(NonZeroU64::MIN),
        }
    }
}

impl GrpcServerConfig {
    /// If TLS is enabled, reads the certificate and key files and returns a valid
    /// `ServerTlsConfig`. If TLS is not enabled, returns `Ok(None)`.
    pub async fn get_valid_tls(&self) -> Result<Option<ServerTlsConfig>, ServerError> {
        match self.tls.clone() {
            Some(tls) => {
                if !tls.cert_path.exists() {
                    return Err(ServerError::ServerConfigError(
                        "TLS enabled but tls_cert_path does not exist".into(),
                    ));
                }
                let cert_path = tls.cert_path;

                if !tls.key_path.exists() {
                    return Err(ServerError::ServerConfigError(
                        "TLS enabled but tls_key_path does not exist".into(),
                    ));
                }
                let key_path = tls.key_path;
                let cert = tokio::fs::read(cert_path).await.map_err(|e| {
                    ServerError::ServerConfigError(format!("Failed to read TLS certificate: {e}"))
                })?;
                let key = tokio::fs::read(key_path).await.map_err(|e| {
                    ServerError::ServerConfigError(format!("Failed to read TLS key: {e}"))
                })?;
                let tls_id = Identity::from_pem(cert, key);
                let tls_config = ServerTlsConfig::new().identity(tls_id);
                Ok(Some(tls_config))
            }
            None => Ok(None),
        }
    }
}

/// Configuration data for Zaino's JSON RPC server, capable of servicing clients over TCP.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct JsonRpcServerConfig {
    /// Server bind addr.
    pub json_rpc_listen_address: SocketAddr,

    /// Enable cookie-based authentication with a valid `Some(<PathBuf>)` value: Directory to store authentication cookie file.
    /// An empty PathBuf that is still Some will have an emphemeral path assigned to it when zaino loads the config.
    #[serde(default)]
    pub cookie_dir: Option<PathBuf>,
}

impl Default for JsonRpcServerConfig {
    fn default() -> Self {
        Self {
            json_rpc_listen_address: SocketAddr::from(([127, 0, 0, 1], 0)),
            cookie_dir: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A config without the table, or with some keys, takes the defaults for
    /// the rest; an unknown key is refused.
    #[test]
    fn limits_default_per_key_and_refuse_unknown_keys() {
        let config: GrpcServerConfig =
            serde_json::from_value(json!({ "listen_address": "127.0.0.1:8137", "tls": null }))
                .expect("parses without limits");
        assert_eq!(config.limits, GrpcLimits::default());
        let limits: GrpcLimits =
            serde_json::from_value(json!({ "max_concurrent_streams": 8 })).expect("parses");
        assert_eq!(limits.max_concurrent_streams.get(), 8);
        assert_eq!(
            limits.concurrency_limit_per_connection,
            GrpcLimits::default().concurrency_limit_per_connection
        );
        assert!(serde_json::from_value::<GrpcLimits>(json!({ "max_streams": 8 })).is_err());
        assert!(
            serde_json::from_value::<GrpcLimits>(json!({ "max_concurrent_streams": 0 })).is_err()
        );
    }
}
