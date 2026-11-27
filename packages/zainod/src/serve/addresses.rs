//! The addresses of an address-index request, checked as zebrad v7 checks
//! them, for both transports.

use std::collections::HashSet;

use zaino_primitives::types::TransparentAddress;
use zcash_address::{ConversionError, TryFromAddress, ZcashAddress};
use zcash_protocol::consensus::NetworkType;

/// zebrad's bound on the addresses of one request.
pub const MAX_REQUEST_ADDRESSES: usize = 10_000;

/// Why an address request is refused, worded as zebrad words it.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AddressesError {
    /// More than [`MAX_REQUEST_ADDRESSES`] addresses (zebrad's −8).
    #[error("too many addresses: the limit is {MAX_REQUEST_ADDRESSES}")]
    TooMany,
    /// A TEX address, which no index entry can match (zebrad's −5).
    #[error(
        "TEX addresses are not supported by the address index, use the transparent P2PKH \
         address they were derived from"
    )]
    Tex,
    /// A malformed or non-transparent address (zebrad's −5).
    #[error("invalid address {address}: {reason}")]
    Invalid {
        /// The address as sent.
        address: String,
        /// Why it is refused.
        reason: String,
    },
}

/// A TEX address, and nothing else.
struct Tex;

impl TryFromAddress for Tex {
    type Error = core::convert::Infallible;

    fn try_from_tex(
        _network: NetworkType,
        _data: [u8; 20],
    ) -> Result<Self, ConversionError<Self::Error>> {
        Ok(Self)
    }
}

fn is_tex(address: &str) -> bool {
    ZcashAddress::try_from_encoded(address).is_ok_and(|address| address.convert::<Tex>().is_ok())
}

/// The addresses of a request: at most [`MAX_REQUEST_ADDRESSES`], each a
/// transparent P2PKH or P2SH address, each once and in the order sent.
pub fn checked_addresses(addresses: &[String]) -> Result<Vec<TransparentAddress>, AddressesError> {
    if addresses.len() > MAX_REQUEST_ADDRESSES {
        return Err(AddressesError::TooMany);
    }
    let mut seen = HashSet::new();
    let mut checked = Vec::with_capacity(addresses.len());
    for address in addresses {
        let parsed =
            TransparentAddress::try_new(address.as_str()).map_err(|error| {
                match is_tex(address) {
                    true => AddressesError::Tex,
                    false => AddressesError::Invalid {
                        address: address.clone(),
                        reason: error.to_string(),
                    },
                }
            })?;
        if seen.insert(parsed.clone()) {
            checked.push(parsed);
        }
    }
    Ok(checked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const P2PKH: &str = "t1Hsc1LR8yKnbbe3twRp88p6vFfC5t7DLbs";
    const TEX: &str = "textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy";

    fn check(addresses: &[&str]) -> Result<Vec<TransparentAddress>, AddressesError> {
        checked_addresses(
            &addresses
                .iter()
                .map(|a| (*a).to_owned())
                .collect::<Vec<_>>(),
        )
    }

    /// An address is kept once; TEX is refused in zebrad's words; anything
    /// else that is not transparent is named; the list is bounded.
    #[test]
    fn addresses_are_checked_as_zebrad_checks_them() {
        let address = TransparentAddress::try_new(P2PKH).expect("valid");
        assert_eq!(check(&[P2PKH, P2PKH]), Ok(vec![address]));
        assert_eq!(check(&[TEX]), Err(AddressesError::Tex));
        assert_eq!(
            AddressesError::Tex.to_string(),
            "TEX addresses are not supported by the address index, use the transparent P2PKH \
             address they were derived from"
        );
        assert!(matches!(
            check(&["not an address"]),
            Err(AddressesError::Invalid { .. })
        ));
        assert_eq!(
            check(&vec!["x"; MAX_REQUEST_ADDRESSES + 1]),
            Err(AddressesError::TooMany)
        );
    }
}
