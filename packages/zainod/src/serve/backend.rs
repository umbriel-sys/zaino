//! What the served handlers answer from: the service, and the facts about
//! this daemon and its validator that some answers carry.

use std::sync::Arc;
use std::time::Duration;

use zaino_primitives::types::Network;
use zaino_service::{Service, ServiceSource};

/// The service and the daemon's facts, shared by every handler.
pub struct Backend<S: ServiceSource> {
    /// The indexer service every read goes through.
    pub service: Arc<Service<S>>,
    /// What this daemon is and how it serves.
    pub info: Arc<ServeInfo>,
}

impl<S: ServiceSource> Clone for Backend<S> {
    fn clone(&self) -> Self {
        Self {
            service: Arc::clone(&self.service),
            info: Arc::clone(&self.info),
        }
    }
}

/// What this daemon is, and the bounds its streams are served under.
#[derive(Debug, Clone)]
pub struct ServeInfo {
    /// The network served.
    pub network: Network,
    /// This build.
    pub build: BuildInfo,
    /// The validator's build, as its `getinfo` reports it.
    pub validator_build: String,
    /// The validator's subversion, as its `getinfo` reports it.
    pub validator_subversion: String,
    /// The operator's donation address, if configured.
    pub donation_address: Option<String>,
    /// How long a stream may wait on a client that is not reading.
    pub stream_timeout: Duration,
    /// How many items a stream buffers ahead of its client.
    pub stream_channel_size: usize,
}

/// This build of zainod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildInfo {
    /// The crate version.
    pub version: &'static str,
    /// The git commit built.
    pub commit: &'static str,
    /// The git branch built.
    pub branch: &'static str,
    /// `SOURCE_DATE_EPOCH`, or `unknown`.
    pub build_date: &'static str,
    /// The user who built it.
    pub build_user: &'static str,
}

impl BuildInfo {
    /// This build, from the values the build script records.
    pub const fn current() -> Self {
        Self {
            version: env!("CARGO_PKG_VERSION"),
            commit: env!("GIT_COMMIT"),
            branch: env!("BRANCH"),
            build_date: env!("BUILD_DATE"),
            build_user: env!("BUILD_USER"),
        }
    }
}
