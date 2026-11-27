//! Common types used across jsonrpsee responses

pub mod amount;

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// The identifier for a Zcash node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct NodeId(pub i64);

/// The height of a Zcash block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct BlockHeight(pub u32);

impl From<u32> for BlockHeight {
    fn from(v: u32) -> Self {
        BlockHeight(v)
    }
}

/// Unix timestamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UnixTime(pub i64);

impl UnixTime {
    /// Converts to a [`SystemTime`]; `None` past what the platform holds.
    pub fn as_system_time(self) -> Option<SystemTime> {
        let offset = Duration::from_secs(self.0.unsigned_abs());
        match self.0 >= 0 {
            true => UNIX_EPOCH.checked_add(offset),
            false => UNIX_EPOCH.checked_sub(offset),
        }
    }
}

/// Duration in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SecondsF64(pub f64);

impl SecondsF64 {
    /// Converts to a [`Duration`].
    pub fn as_duration(self) -> Duration {
        Duration::from_secs_f64(self.0)
    }
}

/// Protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub i64);

/// A byte array.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Bytes(pub u64);

/// Time offset in seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TimeOffsetSeconds(pub i64);
