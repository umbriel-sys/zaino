//! Zcash consensus constants, and the protocol-limit validation built on them.
//!
//! Nothing else in the workspace should restate these values — reference this
//! crate instead.
//!
//! # Why this is its own crate, and why it has no dependencies
//!
//! These are protocol facts. Anything reasoning about the chain needs them,
//! including subsystems built to depend on as little as possible, so holding
//! them in a general-purpose crate meant referencing a reorg bound cost a
//! dependency on the config, logging and TLS stacks too.
//!
//! They are also *not* any implementation's values. A node implementation
//! encodes the consensus rules, exactly as this crate does; it does not define
//! them. Depending on one to learn a protocol constant would invert that —
//! taking a dependency on a peer's reading of a specification we can read
//! ourselves, and dragging that peer's entire type system along for a `u32`.
//!
//! So each value is stated here with its provenance, and
//! `zaino-convert-zebra` — which owns our relationship to zebra's types —
//! carries tests asserting our reading and zebra's still agree. Divergence
//! becomes a test failure rather than a silent behaviour change, without
//! anything having to depend on zebra to obtain a number.

// production code never panics: no indexing, unchecked arithmetic or panicking calls
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::string_slice,
        clippy::get_unwrap,
        clippy::let_underscore_must_use,
        let_underscore_drop
    )
)]

/// Number of confirmations before a coinbase output becomes spendable.
///
/// Zcash protocol specification §3.10: a coinbase output cannot be spent until
/// 100 blocks have been mined on top of the block containing it.
pub const COINBASE_MATURITY: u32 = 100;

/// The protocol's reorganisation limit: no valid reorg rewrites more than this
/// many blocks.
pub const MAX_BLOCK_REORG_HEIGHT: u32 = 1000;

/// Distance below the best-chain tip of the finalised / non-finalised seam: a block
/// buried deeper than this is finalised (reorg-stable).
///
/// [`MAX_BLOCK_REORG_HEIGHT`] plus one for the tip block itself, preserving the
/// historical seam semantics.
pub const MAX_NONFINALISED_DEPTH: u32 = MAX_BLOCK_REORG_HEIGHT + 1;

/// A tractable one-tenth of [`MAX_NONFINALISED_DEPTH`], for fast tests that need a
/// finalised seam without building a full ~[`MAX_NONFINALISED_DEPTH`]-block chain.
///
/// Integer division, so this is `100` when the real depth is `1001`. Test-only: it
/// lets tests select a shallow seam that still derives from the protocol constant
/// rather than a literal of its own.
pub const FAST_TEST_MAX_NONFINALISED_DEPTH: u32 = MAX_NONFINALISED_DEPTH / 10;

/// Blocks a median time is taken over: protocol §7.7.3, `PoWMedianBlockSpan`.
///
/// No zebra agreement test: zebra-state keeps its `POW_MEDIAN_BLOCK_SPAN`
/// `pub(crate)`. The getblockdeltas contract (mock, zebrad, zcashd) checks
/// `mediantime` end to end.
pub const MEDIAN_TIME_SPAN: usize = 11;

/// The median of the last [`MEDIAN_TIME_SPAN`] of `times` (oldest first), as
/// protocol §7.7.3's `MedianTime`: `sorted(S)[ceiling((len(S)+1)/2)]`,
/// 1-based. `None` when `times` is empty.
///
/// A block's `mediantime` (zcashd's `GetMedianTimePast`) is this over the
/// block's time and its ten predecessors'; near genesis, over those that exist.
pub fn median_time_past(times: &[u32]) -> Option<u32> {
    let mut window: Vec<u32> = times
        .iter()
        .skip(times.len().saturating_sub(MEDIAN_TIME_SPAN))
        .copied()
        .collect();
    window.sort_unstable();
    window.get(window.len() / 2).copied()
}

/// Why a client's raw transaction was rejected before it reached a validator.
///
/// A local rejection: nothing here needs the chain, only the protocol's size
/// limit and hex encoding. Callers map it onto whatever their interface's error
/// vocabulary is — the legacy full-node legacy codes live in the serving layer, not here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RawTransactionError {
    /// The submitted string is not valid hex.
    #[error("invalid hex")]
    InvalidHex,

    /// The decoded transaction exceeds the protocol's maximum size.
    #[error(
        "transaction size {size} bytes exceeds maximum allowed size of {MAX_BLOCK_BYTES} bytes"
    )]
    TooLarge {
        /// Size of the decoded transaction, in bytes.
        size: usize,
    },
}

/// Consensus branch ids (protocol §7.1, ZIP 200 and each upgrade's ZIP).
/// From Heartwood each epoch has a ZIP 221 chain history tree: V1 nodes for
/// Heartwood and Canopy, V2 for NU5 to NU6.2 (Orchard), V3 for NU6.3 and NU7
/// (Ironwood). Overwinter, Sapling and Blossom have none.
pub mod branch_id {
    /// Overwinter.
    pub const OVERWINTER: u32 = 0x5ba8_1b19;
    /// Sapling.
    pub const SAPLING: u32 = 0x76b8_09bb;
    /// Blossom.
    pub const BLOSSOM: u32 = 0x2bb4_0e60;
    /// Heartwood: the first epoch with a chain history tree.
    pub const HEARTWOOD: u32 = 0xf5b9_230b;
    /// Canopy.
    pub const CANOPY: u32 = 0xe9ff_75a6;
    /// NU5: Orchard, history nodes V2.
    pub const NU5: u32 = 0xc2d6_d0b4;
    /// NU6.
    pub const NU6: u32 = 0xc8e7_1055;
    /// NU6.1.
    pub const NU6_1: u32 = 0x4dec_4df0;
    /// NU6.2.
    pub const NU6_2: u32 = 0x5437_f330;
    /// NU6.3: Ironwood, history nodes V3.
    pub const NU6_3: u32 = 0x37a5_165b;
    /// NU7 (ZIP 259).
    pub const NU7: u32 = 0x7719_0ad9;
}

/// Maximum serialised size of a block, in bytes.
///
/// Also the maximum size of a transaction: a transaction must fit in a block,
/// so the block limit bounds both.
pub const MAX_BLOCK_BYTES: u64 = 2_000_000;

/// Validates that `bytes` does not exceed the protocol transaction size limit.
pub fn validate_raw_transaction_bytes(bytes: &[u8]) -> Result<(), RawTransactionError> {
    if bytes.len() > MAX_BLOCK_BYTES as usize {
        return Err(RawTransactionError::TooLarge { size: bytes.len() });
    }
    Ok(())
}

/// Validates hex encoding and decoded transaction size before forwarding to a
/// validator.
///
/// Cheap and local, so it runs before the network round trip: an oversized or
/// malformed submission is rejected here rather than costing a validator call.
pub fn validate_raw_transaction_hex(raw_transaction_hex: &str) -> Result<(), RawTransactionError> {
    let bytes = hex::decode(raw_transaction_hex).map_err(|_| RawTransactionError::InvalidHex)?;
    validate_raw_transaction_bytes(&bytes)
}

#[cfg(test)]
mod raw_transaction_tests {
    use super::*;

    #[test]
    fn rejects_invalid_hex() {
        assert_eq!(
            validate_raw_transaction_hex("notahexstring"),
            Err(RawTransactionError::InvalidHex)
        );
    }

    /// An odd-length string is not a rejection of the *transaction* but of the
    /// encoding, and must report as such — a client that sent a truncated hex
    /// string needs to know that, not that its transaction was too big.
    #[test]
    fn rejects_odd_length_hex() {
        assert_eq!(
            validate_raw_transaction_hex("abc"),
            Err(RawTransactionError::InvalidHex)
        );
    }

    #[test]
    fn rejects_oversized_decoded_transaction() {
        let oversized = hex::encode(vec![0u8; MAX_BLOCK_BYTES as usize + 1]);

        assert_eq!(
            validate_raw_transaction_hex(&oversized),
            Err(RawTransactionError::TooLarge {
                size: MAX_BLOCK_BYTES as usize + 1
            })
        );
    }

    /// The limit is inclusive: a transaction of exactly the maximum size is
    /// valid, and rejecting it would refuse a transaction the network accepts.
    #[test]
    fn accepts_max_size_transaction() {
        let max_size = hex::encode(vec![0u8; MAX_BLOCK_BYTES as usize]);

        assert_eq!(validate_raw_transaction_hex(&max_size), Ok(()));
    }

    #[test]
    fn validate_raw_transaction_bytes_rejects_oversized() {
        let oversized = vec![0u8; MAX_BLOCK_BYTES as usize + 1];

        assert!(validate_raw_transaction_bytes(&oversized).is_err());
    }
}

#[cfg(test)]
mod median_time_tests {
    use super::*;

    #[test]
    fn no_times_have_no_median() {
        assert_eq!(median_time_past(&[]), None);
    }

    /// Timestamps are not monotonic; the median sorts them.
    #[test]
    fn an_odd_window_takes_the_middle_of_the_sorted_times() {
        assert_eq!(median_time_past(&[50, 10, 40, 20, 30]), Some(30));
    }

    /// `ceiling((len+1)/2)`, 1-based, is the upper middle of an even window.
    #[test]
    fn an_even_window_takes_the_upper_middle() {
        assert_eq!(median_time_past(&[40, 10, 30, 20]), Some(30));
    }

    /// Only the last eleven count: the older times are outside the span.
    #[test]
    fn only_the_last_eleven_times_count() {
        let mut times = vec![1_000; 5];
        times.extend(1..=11);
        assert_eq!(median_time_past(&times), Some(6));
    }
}
