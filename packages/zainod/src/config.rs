//! Zainod's configuration: the validator it reads, the indexes it builds,
//! where they are stored, and what it serves.
//!
//! Sources are layered, highest priority first: environment variables
//! (`ZAINO_`, `__` between nesting levels), the TOML file, the defaults.

use std::{
    net::{IpAddr, SocketAddr},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use zaino_chain_store_service::{ChainStoreServiceConfig, IndexSet, OnCorruption, SetError};
pub use zaino_chain_store_service::{IndexesConfig, IngestConfig, NamedSet};
use zaino_indexes::IndexId;
use zaino_persistence_lsm_tree::{fs::RealFs, LsmConfig, LsmTreeStorage, DEFAULT_WRITE_BUFFER_MIB};
use zaino_primitives::types::Network;
use zaino_rpc::net::{try_resolve_address, AddressResolution};
use zaino_service::{DiskStore, Mode, ServiceConfig, Store};

use crate::error::IndexerError;
use crate::serve::config::{GrpcServerConfig, JsonRpcServerConfig};

pub mod paths;

/// How deep the chain head runs below the tip, the finalised seam: the
/// consensus reorg bound, or a tenth of it under the test-only
/// `fast-test-seam` feature.
#[cfg(not(feature = "fast-test-seam"))]
pub(crate) const NONFINALISED_DEPTH: u32 = zaino_consensus::MAX_NONFINALISED_DEPTH;
#[cfg(feature = "fast-test-seam")]
pub(crate) const NONFINALISED_DEPTH: u32 = zaino_consensus::FAST_TEST_MAX_NONFINALISED_DEPTH;

/// The header `generate-config` writes above the defaults.
pub const GENERATED_CONFIG_HEADER: &str = r#"# Zaino configuration
#
# Generated with `zainod generate-config`. Every setting is listed here:
# active lines show the default, commented lines are optional.
#
# Sources, highest priority first:
#   1. Environment: ZAINO_<SECTION>__<KEY>, e.g. ZAINO_VALIDATOR__RPC_ADDRESS.
#      Keys ending in password, secret, token, cookie or private_key are
#      refused from the environment: put them in this file.
#   2. This file.
#   3. Built-in defaults.
#
# Decide these first:
#   - network                      which chain to serve
#   - [validator]                  where your validator is, and how to read it
#   - [indexes] set                what Zaino stores (decides what it serves fast)
#   - [persistence] path           where the indexes live on disk
#
# Every index and the JSON-RPC server: see `zainod-full.toml`,
#   https://github.com/umbriel-sys/zaino/blob/dev/docs/example_configs/zainod-full.toml
#
# Documentation: https://github.com/umbriel-sys/zaino
"#;

/// Every setting with its default, commented; the two machine paths are
/// filled in by [`generate_default_config`].
const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("config/default.toml");

/// Top-level keys of the configuration before the service rewrite, and what
/// each became.
const OLD_KEYS: [(&str, &str); 9] = [
    ("backend", "validator.kind"),
    ("zebra_db_path", "validator.read_state.state_path"),
    ("ephemeral_finalised_state", "indexes.set = \"empty\""),
    ("metrics_endpoint", "admin.listen_address"),
    ("json_server_settings", "serve.jsonrpc"),
    ("grpc_settings", "serve.grpc"),
    ("validator_settings", "validator"),
    (
        "service",
        "serve.stream_timeout_secs and serve.stream_channel_size",
    ),
    ("storage", "persistence"),
];

/// Environment keys that may not carry a value: secrets belong in the file.
const SENSITIVE_KEY_SUFFIXES: [&str; 5] = ["password", "secret", "token", "cookie", "private_key"];

/// Everything zainod is configured with.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ZainodConfig {
    /// The network served.
    pub network: Network,
    /// The operator's donation address, reported by `GetLightdInfo`.
    pub donation_address: Option<String>,
    /// The validator read from.
    pub validator: ValidatorConfig,
    /// The indexes built.
    pub indexes: IndexesConfig,
    /// Where the indexes are stored.
    pub persistence: PersistenceConfig,
    /// How hard the store's build pipeline works.
    pub ingest: IngestConfig,
    /// The mempool's bounds.
    pub mempool: MempoolSettings,
    /// What is served.
    pub serve: ServeConfig,
    /// The admin endpoint (`/livez`, `/readyz`, `/metrics`); absent = none.
    pub admin: Option<AdminConfig>,
}

impl Default for ZainodConfig {
    fn default() -> Self {
        Self {
            network: Network::Mainnet,
            donation_address: None,
            validator: ValidatorConfig::default(),
            indexes: IndexesConfig {
                set: Some(NamedSet::Lightclient),
                ..IndexesConfig::default()
            },
            persistence: PersistenceConfig::default(),
            ingest: IngestConfig::default(),
            mempool: MempoolSettings::default(),
            serve: ServeConfig::default(),
            admin: None,
        }
    }
}

/// Which source reads the validator.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorKind {
    /// A zebra on this host: its JSON-RPC and its state directory
    /// (`[validator.read_state]` is required).
    #[default]
    Zebra,
    /// Any validator serving the zcash JSON-RPC interface.
    ZcashRpc,
}

/// The validator read from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ValidatorConfig {
    /// Which source reads it.
    pub kind: ValidatorKind,
    /// Its JSON-RPC address (`host:port`); a private address.
    pub rpc_address: String,
    /// Its cookie file; wins over `user` / `password`.
    pub cookie_path: Option<PathBuf>,
    /// Its RPC user; with no cookie and no pair, no credentials are sent.
    pub user: Option<String>,
    /// Its RPC password.
    pub password: Option<String>,
    /// Where zebra's state is read directly: `kind = "zebra"` only. A
    /// `[validator]` table without it has none, whatever the default.
    #[serde(default)]
    pub read_state: Option<ReadStateConfig>,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            kind: ValidatorKind::default(),
            rpc_address: "127.0.0.1:8232".to_string(),
            cookie_path: None,
            user: None,
            password: None,
            read_state: Some(ReadStateConfig {
                state_path: paths::cache_path("zebra"),
                grpc_address: "127.0.0.1:8230".to_string(),
            }),
        }
    }
}

/// Where zebra's state is read directly.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReadStateConfig {
    /// zebra's state cache directory.
    pub state_path: PathBuf,
    /// zebra's indexer gRPC address (`host:port`).
    pub grpc_address: String,
}

/// Where the indexes are stored.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct PersistenceConfig {
    /// The data directory; the store is `<path>/<network>/`.
    pub path: PathBuf,
    /// Buffered MiB at which a commit writes the buffer.
    pub write_buffer_mib: NonZeroUsize,
    /// Each index's pool threads; absent = the machine's, shared out.
    pub threads: Option<NonZeroUsize>,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            path: paths::cache_path("zaino"),
            write_buffer_mib: NonZeroUsize::new(DEFAULT_WRITE_BUFFER_MIB)
                .unwrap_or(NonZeroUsize::MIN),
            threads: None,
        }
    }
}

/// The mempool's bounds; each absent key takes the mempool's own default.
#[derive(Debug, Default, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct MempoolSettings {
    /// Most memory the mempool may hold, as total ZIP-401 cost in bytes.
    pub max_cost_bytes: Option<u64>,
    /// How often the validator's mempool is polled, in milliseconds.
    pub poll_interval_ms: Option<u64>,
    /// The least time between two whole-mempool metadata listings.
    pub metadata_min_interval_ms: Option<u64>,
    /// Most txid suffixes a `GetMempoolTx` exclude list may hold.
    pub max_exclude_count: Option<usize>,
}

impl MempoolSettings {
    fn to_mempool_config(&self) -> zaino_mempool::MempoolConfig {
        let mut config = zaino_mempool::MempoolConfig::default();
        if let Some(max_cost_bytes) = self.max_cost_bytes {
            config.set_max_cost_bytes(max_cost_bytes);
        }
        if let Some(poll_interval_ms) = self.poll_interval_ms.and_then(std::num::NonZeroU64::new) {
            config.set_poll_interval_ms(poll_interval_ms);
            // the listing floor follows the poll cadence unless set itself
            config.set_metadata_min_interval(config.poll_interval());
        }
        if let Some(metadata_min_interval_ms) = self.metadata_min_interval_ms {
            config.set_metadata_min_interval(Duration::from_millis(metadata_min_interval_ms));
        }
        if let Some(max_exclude_count) = self.max_exclude_count {
            config.set_max_exclude_count(max_exclude_count);
        }
        config
    }
}

/// What is served.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServeConfig {
    /// Seconds a stream waits on a client that is not reading.
    pub stream_timeout_secs: u32,
    /// Items a stream buffers ahead of its client.
    pub stream_channel_size: u32,
    /// The gRPC server.
    pub grpc: GrpcServerConfig,
    /// The JSON-RPC server; absent = not served.
    pub jsonrpc: Option<JsonRpcServerConfig>,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self {
            stream_timeout_secs: 30,
            stream_channel_size: 32,
            grpc: GrpcServerConfig {
                listen_address: SocketAddr::from(([127, 0, 0, 1], 8137)),
                tls: None,
                limits: Default::default(),
            },
            jsonrpc: None,
        }
    }
}

/// The admin endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    /// Where `/livez`, `/readyz` and `/metrics` are served.
    pub listen_address: SocketAddr,
}

impl ZainodConfig {
    /// The service's config: no store when no index is named, else a
    /// primary store of the named set.
    pub fn service_config(&self) -> ServiceConfig {
        let builds_nothing = matches!(self.indexes.set, None | Some(NamedSet::Empty))
            && self.indexes.enable.is_empty();
        let store = match builds_nothing {
            true => Store::ValidatorBacked {
                network: self.network,
            },
            false => Store::DiskAccelerated(Box::new(DiskStore {
                store: ChainStoreServiceConfig {
                    root: self.persistence.path.clone(),
                    network: self.network,
                    indexes: self.indexes.clone(),
                    on_corruption: OnCorruption::default(),
                    ingest: self.ingest.clone(),
                },
                storage: LsmTreeStorage {
                    fs: RealFs::shared(),
                    lsm: LsmConfig::default(),
                    write_buffer: self
                        .persistence
                        .write_buffer_mib
                        .get()
                        .checked_mul(1 << 20)
                        .and_then(NonZeroUsize::new)
                        .unwrap_or(NonZeroUsize::MAX),
                    threads: self.persistence.threads,
                    strict_manifest_read: false,
                },
                mode: Mode::Primary,
            })),
        };
        ServiceConfig {
            store,
            chain_view: zaino_chain::ChainViewConfig::default(),
            chain_head: zaino_chain_head::ChainHeadConfig::with_max_depth(
                std::num::NonZeroU32::new(NONFINALISED_DEPTH).unwrap_or(std::num::NonZeroU32::MIN),
            ),
            mempool: self.mempool.to_mempool_config(),
        }
    }

    /// Refuses a configuration that cannot run as written.
    pub(crate) fn check_config(&self) -> Result<(), IndexerError> {
        self.check_validator()?;
        self.check_servers()?;
        self.check_mempool()?;
        self.check_indexes()?;
        self.check_paths(paths::cache_base_known())?;
        if let Some(address) = &self.donation_address {
            zcash_address::ZcashAddress::try_from_encoded(address).map_err(|error| {
                config_error(format!(
                    "donation_address '{address}' is not valid: {error}"
                ))
            })?;
        }
        Ok(())
    }

    /// A store or state path left to its default needs a cache directory
    /// (`cache_base_known`): the `/tmp` fallback would lose the store at a
    /// reboot.
    fn check_paths(&self, cache_base_known: bool) -> Result<(), IndexerError> {
        if cache_base_known {
            return Ok(());
        }
        let unset = |key: &str| {
            config_error(format!(
                "{key} is not set, and neither XDG_CACHE_HOME nor HOME is set to give it a \
                 default: set {key}"
            ))
        };
        if self.persistence.path == paths::cache_path("zaino") {
            return Err(unset("persistence.path"));
        }
        match &self.validator.read_state {
            Some(read_state) if read_state.state_path == paths::cache_path("zebra") => {
                Err(unset("validator.read_state.state_path"))
            }
            _ => Ok(()),
        }
    }

    fn check_validator(&self) -> Result<(), IndexerError> {
        let validator = &self.validator;
        match (validator.kind, &validator.read_state) {
            (ValidatorKind::Zebra, None) => {
                return Err(config_error(
                    "validator.kind = \"zebra\" reads zebra's state: set \
                     [validator.read_state] (state_path and grpc_address), or use \
                     kind = \"zcash_rpc\"",
                ))
            }
            (ValidatorKind::ZcashRpc, Some(_)) => {
                return Err(config_error(
                    "[validator.read_state] is zebra's: set validator.kind = \"zebra\", or \
                     remove the table",
                ))
            }
            (ValidatorKind::Zebra, Some(read_state)) => {
                resolve(
                    &read_state.grpc_address,
                    "validator.read_state.grpc_address",
                )?;
            }
            (ValidatorKind::ZcashRpc, None) => {}
        }
        if validator.user.is_some() != validator.password.is_some() {
            return Err(config_error(
                "validator.user and validator.password are set together or not at all",
            ));
        }
        if let Some(cookie_path) = &validator.cookie_path {
            if !cookie_path.exists() {
                return Err(config_error(format!(
                    "validator.cookie_path {cookie_path:?} does not exist"
                )));
            }
        }
        // a resolved address must be private; a hostname is resolved when
        // connecting (container service discovery)
        match try_resolve_address(&validator.rpc_address) {
            AddressResolution::Resolved(address) if !is_private_listen_addr(&address) => Err(
                config_error("validator.rpc_address must be a private or loopback address"),
            ),
            AddressResolution::Resolved(_) => Ok(()),
            AddressResolution::UnresolvedHostname { address, .. } => {
                info!(%address, "validator address cannot be resolved at config time");
                Ok(())
            }
            AddressResolution::InvalidFormat { address, reason } => Err(config_error(format!(
                "validator.rpc_address '{address}' is not valid: {reason}"
            ))),
        }
    }

    fn check_servers(&self) -> Result<(), IndexerError> {
        let grpc = &self.serve.grpc;
        if let Some(tls) = &grpc.tls {
            for (key, path) in [("cert_path", &tls.cert_path), ("key_path", &tls.key_path)] {
                if !path.exists() {
                    return Err(config_error(format!(
                        "serve.grpc.tls.{key} {path:?} does not exist"
                    )));
                }
            }
        }
        #[cfg(not(feature = "no_tls_use_unencrypted_traffic"))]
        if !is_private_listen_addr(&grpc.listen_address) && grpc.tls.is_none() {
            return Err(config_error(
                "serve.grpc.listen_address is public: [serve.grpc.tls] is required",
            ));
        }
        #[cfg(feature = "no_tls_use_unencrypted_traffic")]
        warn!("built with no_tls_use_unencrypted_traffic: gRPC may be served unencrypted");

        if let Some(jsonrpc) = &self.serve.jsonrpc {
            // JSON-RPC has no transport encryption
            #[cfg(not(feature = "allow_unencrypted_public_json_rpc_bind"))]
            if !is_private_listen_addr(&jsonrpc.listen_address) {
                return Err(config_error(
                    "serve.jsonrpc.listen_address must be private or loopback (build with \
                     `allow_unencrypted_public_json_rpc_bind` to override, on trusted \
                     private networks only)",
                ));
            }
            #[cfg(feature = "allow_unencrypted_public_json_rpc_bind")]
            warn!("built with allow_unencrypted_public_json_rpc_bind: JSON-RPC may be public");
            if jsonrpc.listen_address == grpc.listen_address {
                return Err(config_error(
                    "serve.grpc and serve.jsonrpc must listen on different addresses",
                ));
            }
        }
        if let Some(admin) = &self.admin {
            if !is_private_listen_addr(&admin.listen_address) {
                warn!(
                    endpoint = %admin.listen_address,
                    "admin.listen_address is public; /metrics is unauthenticated and exposes \
                     operational detail"
                );
            }
        }
        Ok(())
    }

    /// The index set the store would refuse at boot is refused here, before
    /// anything is connected, naming the key to change.
    fn check_indexes(&self) -> Result<(), IndexerError> {
        let indexes = &self.indexes;
        match IndexSet::resolve(indexes.set, &indexes.enable, &indexes.disable) {
            Ok(_) => Ok(()),
            Err(SetError::UnknownIndex(name)) => {
                let key = match indexes.enable.contains(&name) {
                    true => "enable",
                    false => "disable",
                };
                let known: Vec<&str> = IndexId::ALL.iter().map(|id| id.name()).collect();
                Err(config_error(format!(
                    "indexes.{key} names `{name}`, which is not an index. The indexes are: {}",
                    known.join(", ")
                )))
            }
            Err(SetError::DisabledDependency {
                disabled,
                needed_by,
            }) => Err(config_error(format!(
                "indexes.disable names `{disabled}`, but `{needed_by}` is enabled and depends \
                 on it. Remove `{disabled}` from indexes.disable, or add `{needed_by}` to \
                 indexes.disable as well"
            ))),
        }
    }

    fn check_mempool(&self) -> Result<(), IndexerError> {
        if let Some(max_cost_bytes) = self.mempool.max_cost_bytes {
            let floor = zaino_mempool::config::MEMPOOL_TRANSACTION_COST_THRESHOLD;
            if max_cost_bytes < floor {
                return Err(config_error(format!(
                    "mempool.max_cost_bytes ({max_cost_bytes}) is below the ZIP-401 \
                     per-transaction floor ({floor}): it cannot admit one transaction"
                )));
            }
        }
        // a zero period aborts the timer at spawn
        if self.mempool.poll_interval_ms == Some(0) {
            return Err(config_error(
                "mempool.poll_interval_ms must be greater than zero",
            ));
        }
        Ok(())
    }
}

fn config_error(message: impl Into<String>) -> IndexerError {
    IndexerError::ConfigError(message.into())
}

/// `address` as a socket address; `key` names it in the error.
pub(crate) fn resolve(address: &str, key: &str) -> Result<SocketAddr, IndexerError> {
    zaino_rpc::net::resolve_socket_addr(address)
        .map_err(|error| config_error(format!("{key} '{address}' is not valid: {error}")))
}

/// Whether `addr` is private (RFC 1918 / RFC 4193) or loopback.
pub(crate) fn is_private_listen_addr(addr: &SocketAddr) -> bool {
    match addr.ip() {
        IpAddr::V4(ip) => ip.is_private() || ip.is_loopback(),
        IpAddr::V6(ip) => ip.is_unique_local() || ip.is_loopback(),
    }
}

/// The default cookie directory for the JSON-RPC server.
pub fn default_ephemeral_cookie_path() -> PathBuf {
    paths::runtime_path("zaino/.cookie")
}

/// The default configuration as commented TOML, under
/// [`GENERATED_CONFIG_HEADER`].
pub fn generate_default_config() -> Result<String, IndexerError> {
    let default = ZainodConfig::default();
    let state_path = default
        .validator
        .read_state
        .map(|read_state| read_state.state_path)
        .ok_or_else(|| config_error("the default validator has no read state"))?;
    let content = DEFAULT_CONFIG_TEMPLATE
        .replace("{state_path}", &toml_path(&state_path)?)
        .replace("{persistence_path}", &toml_path(&default.persistence.path)?);
    Ok(format!("{GENERATED_CONFIG_HEADER}{content}"))
}

/// `path` as a quoted TOML string.
fn toml_path(path: &Path) -> Result<String, IndexerError> {
    let path = path
        .to_str()
        .ok_or_else(|| config_error(format!("the default path {path:?} is not UTF-8")))?;
    Ok(toml::Value::String(path.to_owned()).to_string())
}

/// Loads the configuration at `file_path`, layered under `ZAINO_`
/// environment variables.
pub fn load_config(file_path: &Path) -> Result<ZainodConfig, IndexerError> {
    load_config_with_env(file_path, "ZAINO")
}

/// [`load_config`] with the environment prefix given.
pub fn load_config_with_env(
    file_path: &Path,
    env_prefix: &str,
) -> Result<ZainodConfig, IndexerError> {
    let required_prefix = format!("{env_prefix}_");
    for (key, _) in std::env::vars() {
        let leaf = key
            .strip_prefix(&required_prefix)
            .and_then(|key| key.split("__").last());
        if let Some(leaf) = leaf.filter(|leaf| is_sensitive_leaf_key(leaf)) {
            return Err(config_error(format!(
                "environment variable '{key}' carries the sensitive key '{leaf}': use the \
                 config file"
            )));
        }
    }

    let settings = config::Config::builder()
        .add_source(
            config::File::from(file_path)
                .format(config::FileFormat::Toml)
                .required(true),
        )
        .add_source(
            config::Environment::with_prefix(env_prefix)
                .prefix_separator("_")
                .separator("__")
                .try_parsing(true),
        )
        .build()
        .map_err(|error| config_error(format!("configuration loading failed: {error}")))?;

    // an old key is named, with what replaced it, before anything is parsed
    let keys = settings
        .clone()
        .try_deserialize::<toml::Table>()
        .map_err(|error| config_error(format!("configuration parsing failed: {error}")))?;
    for (key, replacement) in OLD_KEYS {
        if keys.contains_key(key) {
            return Err(IndexerError::OldConfigKey { key, replacement });
        }
    }

    let mut parsed: ZainodConfig = settings
        .try_deserialize()
        .map_err(|error| config_error(format!("configuration parsing failed: {error}")))?;
    // an empty cookie directory means the default one
    if let Some(jsonrpc) = &mut parsed.serve.jsonrpc {
        if jsonrpc
            .cookie_dir
            .as_ref()
            .is_some_and(|dir| dir.as_os_str().is_empty())
        {
            jsonrpc.cookie_dir = Some(default_ephemeral_cookie_path());
        }
    }
    parsed.check_config()?;
    info!(path = %file_path.display(), "config loaded and validated");
    Ok(parsed)
}

fn is_sensitive_leaf_key(leaf_key: &str) -> bool {
    let key = leaf_key.to_ascii_lowercase();
    SENSITIVE_KEY_SUFFIXES
        .iter()
        .any(|suffix| key.ends_with(suffix))
}

#[cfg(test)]
mod tests;
