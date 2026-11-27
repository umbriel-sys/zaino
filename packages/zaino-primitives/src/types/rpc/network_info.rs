//! `getnetworkinfo`: the validator's peer-to-peer network view.

use crate::types::Zatoshis;

/// The validator's network state, as `getnetworkinfo` reports it; relayed
/// whole, Zaino indexes none of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkInfo {
    /// The validator's version, in its own numeric encoding.
    pub version: u64,
    /// The network user agent, e.g. `"/Zebra:7.0.0/"`.
    pub subversion: String,
    /// The peer-to-peer protocol version.
    pub protocol_version: u32,
    /// The service flags it offers, hex-encoded as the validator reports them.
    pub local_services: String,
    /// Its clock's offset from its peers, in seconds.
    pub time_offset: i64,
    /// Its peer connections, inbound and outbound.
    pub connections: u64,
    /// Each transport it knows of, and whether it is reachable.
    pub networks: Vec<NetworkEntry>,
    /// The minimum relay fee, in zatoshis per kilobyte.
    pub relay_fee: Zatoshis,
    /// The addresses it advertises for itself.
    pub local_addresses: Vec<LocalAddress>,
    /// Its networking warnings, empty when none.
    pub warnings: String,
}

/// One transport's reachability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkEntry {
    /// `ipv4`, `ipv6` or `onion`.
    pub name: String,
    /// Whether it connects to this network only on request.
    pub limited: bool,
    /// Whether it can reach this network.
    pub reachable: bool,
    /// The proxy it routes this network through, empty when none.
    pub proxy: String,
    /// Whether it randomises credentials per proxy connection.
    pub proxy_randomize_credentials: bool,
}

/// One address the validator advertises.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalAddress {
    /// The address.
    pub address: String,
    /// The port it listens on.
    pub port: u16,
    /// Its confidence in the address.
    pub score: u32,
}
