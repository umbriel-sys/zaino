//! Wire shapes for `validateaddress` and `z_validateaddress`.

use serde::{
    ser::{SerializeMap, SerializeStruct},
    Serialize, Serializer,
};
use zaino_address::ZValidatedAddress;
use zaino_primitives::types::Network;
use zcash_protocol::consensus::{
    BlockHeight, NetworkType, NetworkUpgrade, Parameters, MAIN_NETWORK, TEST_NETWORK,
};

/// The network an address is classified for. `zaino-address` reads only the
/// network type.
#[derive(Clone, Copy)]
pub(crate) struct AddressNetwork(pub(crate) Network);

impl Parameters for AddressNetwork {
    fn network_type(&self) -> NetworkType {
        match self.0 {
            Network::Mainnet => NetworkType::Main,
            Network::PubTestnet => NetworkType::Test,
            Network::Regtest => NetworkType::Regtest,
        }
    }

    /// Regtest's schedule is its validator's, which an address does not
    /// depend on: `None`.
    fn activation_height(&self, upgrade: NetworkUpgrade) -> Option<BlockHeight> {
        match self.0 {
            Network::Mainnet => MAIN_NETWORK.activation_height(upgrade),
            Network::PubTestnet => TEST_NETWORK.activation_height(upgrade),
            Network::Regtest => None,
        }
    }
}

/// The `z_validateaddress` response, in zebrad's shape: `isvalid` alone when
/// invalid, else `isvalid`, `address`, `address_type` and `ismine: false`
/// (neither zebrad nor Zaino holds a wallet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ZValidateAddressWire {
    /// Serializes as `{"isvalid": false}` and nothing else.
    Invalid,

    /// Serializes with the address and its kind.
    Valid(ValidZValidateAddressWire),
}

impl ZValidateAddressWire {
    /// Renders a [`ZValidatedAddress`] into the wire shape.
    pub fn from_domain(validated: ZValidatedAddress) -> Self {
        let (address, kind) = match validated {
            ZValidatedAddress::Invalid => return Self::Invalid,
            ZValidatedAddress::P2pkh { address } => (address, AddressKindWire::P2pkh),
            ZValidatedAddress::P2sh { address } => (address, AddressKindWire::P2sh),
            ZValidatedAddress::Unified { address } => (address, AddressKindWire::Unified),
            ZValidatedAddress::Sapling { address, .. } => (address, AddressKindWire::Sapling),
        };
        Self::Valid(ValidZValidateAddressWire { address, kind })
    }
}

impl Serialize for ZValidateAddressWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Invalid => {
                let mut invalid = serializer.serialize_struct("ZValidateAddressWire", 1)?;
                invalid.serialize_field("isvalid", &false)?;
                invalid.end()
            }
            Self::Valid(valid) => valid.serialize(serializer),
        }
    }
}

/// The `isvalid: true` branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidZValidateAddressWire {
    /// The address, as reported back to the caller.
    address: String,
    /// Which kind of address it is.
    kind: AddressKindWire,
}

impl Serialize for ValidZValidateAddressWire {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(4))?;
        map.serialize_entry("isvalid", &true)?;
        map.serialize_entry("address", &self.address)?;
        map.serialize_entry("address_type", &self.kind)?;
        map.serialize_entry("ismine", &false)?;
        map.end()
    }
}

/// Address kinds the legacy full node's `z_validateaddress` names.
///
/// There is no `sprout` variant: Zaino does not classify Sprout addresses, so
/// this can never be asked to serialize one. See
/// [`ZValidatedAddress`]'s Sprout note.
#[derive(Copy, Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
enum AddressKindWire {
    P2pkh,
    P2sh,
    Sapling,
    Unified,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn invalid_serializes_to_isvalid_false_only() {
        let wire = ZValidateAddressWire::from_domain(ZValidatedAddress::Invalid);
        assert_eq!(
            serde_json::to_value(&wire).unwrap(),
            json!({ "isvalid": false })
        );
    }

    /// Every valid kind is zebrad's four keys and nothing else (#41): no
    /// legacy `type`, no Sapling key material.
    #[test]
    fn a_valid_address_is_zebrads_shape() {
        let cases = [
            (
                ZValidatedAddress::P2pkh {
                    address: "t1abc".into(),
                },
                "t1abc",
                "p2pkh",
            ),
            (
                ZValidatedAddress::P2sh {
                    address: "t3zzz".into(),
                },
                "t3zzz",
                "p2sh",
            ),
            (
                ZValidatedAddress::Sapling {
                    address: "zs1xx".into(),
                    diversifier: [0xab; 11],
                    diversified_transmission_key: [0xcd; 32],
                },
                "zs1xx",
                "sapling",
            ),
            (
                ZValidatedAddress::Unified {
                    address: "u1blah".into(),
                },
                "u1blah",
                "unified",
            ),
        ];
        for (domain, address, kind) in cases {
            assert_eq!(
                serde_json::to_value(ZValidateAddressWire::from_domain(domain)).unwrap(),
                json!({
                    "isvalid": true,
                    "address": address,
                    "address_type": kind,
                    "ismine": false,
                })
            );
        }
    }
}

/// End-to-end vectors: address string in, served JSON out.
///
/// `z_validateaddress` is the composition of [`zaino_address::z_validate_address`]
/// with [`ZValidateAddressWire::from_domain`] and nothing else, so the whole
/// classification contract is pinned here. The live suite keeps one vector, to
/// prove the endpoint reaches this pair.
#[cfg(test)]
mod served_vectors {
    use super::*;
    use serde_json::{json, Value};

    // Captured from zcashd's `z_validateaddress`.
    const P2PKH: &str = "tmVqEASZxBNKFTbmASZikGa5fPLkd68iJyx";
    const P2SH: &str = "t2MjoXQ2iDrjG9QXNZNCY9io8ecN4FJYK1u";
    const SAPLING: &str = "zregtestsapling1jalqhycwumq3unfxlzyzcktq3n478n82k2wacvl8gwfxk6ahshkxmtp2034qj28n7gl92ka5wca";
    const UNIFIED: &str = "uregtest1njwg60x0jarhyuuxrcdvw854p68cgdfe85822lmclc7z9vy9xqr7t49n3d97k2dwlee82skwwe0ens0rc06p4vr04tvd3j9ckl3qry83ckay4l4ngdq9atg7vuj9z58tfjs0mnsgyrnprtqfv8almu564z498zy6tp2aa569tk8fyhdazyhytel2m32awe4kuy6qq996um3ljaajj36";
    const SPROUT: &str = "ztfhKyLouqi8sSwjRm4YMQdWPjTmrJ4QgtziVQ1Kd1e9EsRHYKofjoJdF438FwcUQnix8yrbSrzPpJJNABewgNffs5d4YZJ";

    fn served(address: &str, network: Network) -> Value {
        serde_json::to_value(ZValidateAddressWire::from_domain(
            zaino_address::z_validate_address(address.to_string(), &AddressNetwork(network)),
        ))
        .expect("ZValidateAddressWire serializes infallibly")
    }

    /// The kind is what a classifier bug would get wrong: pinned for every
    /// kind, in zebrad's shape.
    #[test]
    fn each_kind_serves_its_own_shape() {
        let network = Network::Regtest;

        for (address, kind) in [
            (P2PKH, "p2pkh"),
            (P2SH, "p2sh"),
            (SAPLING, "sapling"),
            (UNIFIED, "unified"),
        ] {
            assert_eq!(
                served(address, network),
                json!({
                    "isvalid": true,
                    "address": address,
                    "address_type": kind,
                    "ismine": false,
                }),
                "{address}"
            );
        }
    }

    /// Sprout is here because Zaino declines to classify it rather than
    /// failing to parse it — the only thing to assert about it is "invalid".
    #[test]
    fn unclassifiable_addresses_serve_isvalid_false_alone() {
        let network = Network::Regtest;

        for address in [
            SPROUT,
            "t1123456789ABCDEFGHJKLMNPQRSTUVWXY",
            "t1000000000000000000000000000000000",
            "not an address",
        ] {
            assert_eq!(
                served(address, network),
                json!({ "isvalid": false }),
                "{address}"
            );
        }
    }

    /// An address well-formed for another network must not validate; the
    /// regtest vectors are rejected on mainnet and vice versa.
    #[test]
    fn network_scoping_rejects_foreign_addresses() {
        let mainnet = Network::Mainnet;

        for address in [P2PKH, P2SH, SAPLING, UNIFIED] {
            assert_eq!(
                served(address, mainnet),
                json!({ "isvalid": false }),
                "{address} is not a mainnet address"
            );
        }
    }
}
