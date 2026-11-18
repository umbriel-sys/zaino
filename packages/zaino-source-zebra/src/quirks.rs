//! What zebrad answers differently from the zcash RPC spec.

use zaino_rpc::RpcClient;
use zaino_source::{FailureMode, NonDomainError};
use zaino_source_zcash_rpc::{Quirks, ZcashRpcAdapter};

/// The zcash RPC adapter reading replies as zebrad sends them.
pub fn rpc_adapter(client: RpcClient) -> ZcashRpcAdapter {
    ZcashRpcAdapter::with_quirks(client, ZEBRAD)
}

/// zebrad's departures from the spec. Its address RPCs parse any network's
/// addresses (zebra-rpc `valid_addresses`), answering another network's empty.
const ZEBRAD: Quirks = Quirks {
    result_cap: Some(result_cap_exceeded),
    unchecked_address_network: true,
};

/// The cap zebrad 7 reports when an address query matches too much.
///
/// zebrad answers `-8` with `more than {cap} … match; …`. `-8` is overloaded
/// (a malformed envelope too), so only that message shape is read as the cap.
fn result_cap_exceeded(error: &NonDomainError) -> Option<usize> {
    if !matches!(error.mode, FailureMode::RpcError(-8)) {
        return None;
    }
    let rest = error.message.strip_prefix("more than ")?;
    let (cap, rest) = rest.split_once(' ')?;
    rest.contains(" match").then(|| cap.parse().ok()).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rpc(code: i64, message: &str) -> NonDomainError {
        zaino_rpc::RpcError::Rpc {
            code,
            message: message.to_string(),
        }
        .into()
    }

    /// Only zebrad 7's `-8 more than {cap} … match` reads as the cap.
    #[test]
    fn zebrads_result_cap_is_read_by_its_message_shape() {
        assert_eq!(
            result_cap_exceeded(&rpc(
                -8,
                "more than 100000 transactions match; narrow the height range"
            )),
            Some(100_000)
        );
        assert_eq!(
            result_cap_exceeded(&rpc(-8, "start must be less than end")),
            None
        );
        assert_eq!(
            result_cap_exceeded(&rpc(-1, "more than 100000 transactions match")),
            None
        );
    }
}
