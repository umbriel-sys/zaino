//! `getpeerinfo` — the backing validator's peer connections.

/// One peer connection held by the backing validator, as zebrad reports it.
///
/// These are the *validator's* peers. Zaino is not a p2p node and has none of
/// its own; it forwards this listing on the validator's behalf.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerInfo {
    /// Remote peer address as the validator reports it.
    ///
    /// Opaque on purpose: usually `host:port`, but peers are also reached
    /// over Tor or I2P, whose addresses are not socket addresses. Zaino
    /// forwards it and never inspects it.
    pub addr: String,

    /// The services the peer offers (written as 16 hex digits).
    pub services: u64,

    /// When the last message from the peer arrived, in seconds since the
    /// Unix epoch.
    pub last_received: u32,

    /// Whether the peer initiated the connection to the validator.
    pub inbound: bool,

    /// The peer's misbehaviour score.
    pub ban_score: u32,

    /// The peer's user agent, e.g. `/Zebra:2.1.0/`.
    pub user_agent: String,

    /// The negotiated protocol version.
    pub version: u32,

    /// The connection's state as the validator names it (`connected`,
    /// `never_connected`, `failed`, `connecting`). An open set: forwarded,
    /// never matched on.
    pub connection_state: String,

    /// The round-trip ping time in seconds; `None` before one completes.
    pub ping_time: Option<f64>,

    /// How long a ping has been outstanding, in seconds; `None` with none.
    pub ping_wait: Option<f64>,
}
