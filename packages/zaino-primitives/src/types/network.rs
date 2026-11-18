//! The network kind, and a regtest activation schedule.

use std::fmt;

/// The network *kind* zaino runs against. Deliberately payload-free:
/// activation heights are chain facts the validator owns, so a config value
/// cannot carry them — the indexer adopts the runtime schedule from the
/// validator's `getblockchaininfo.upgrades` at spawn
/// (<https://github.com/zingolabs/zaino/issues/1076>). A pre-adoption height
/// read is unrepresentable: this type has no heights to read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Deserialize, serde::Serialize))]
pub enum Network {
    /// Mainnet network
    Mainnet,
    /// The Public Testnet: the shared-consensus test network, whose
    /// activation heights come only from checked-in parameters — never from
    /// configuration. Accepts the legacy config spelling `"Testnet"`.
    #[cfg_attr(feature = "serde", serde(alias = "Testnet"))]
    PubTestnet,
    /// Regtest network (for local testing)
    Regtest,
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Network::Mainnet => write!(f, "Mainnet"),
            Network::PubTestnet => write!(f, "PubTestnet"),
            Network::Regtest => write!(f, "Regtest"),
        }
    }
}

impl Network {
    /// Determines if we should wait for the server to fully sync. Used for testing
    ///
    /// - Mainnet / The Public Testnet: Skip sync (false) because we don't want
    ///   to sync real chains in tests
    /// - Regtest: Enable sync (true) because regtest is local and fast to sync
    pub fn wait_on_server_sync(&self) -> bool {
        match self {
            // Real networks - don't try to sync the whole chain
            Network::Mainnet | Network::PubTestnet => false,
            // Local network - safe and fast to sync
            Network::Regtest => true,
        }
    }
}

#[cfg(feature = "zebra")]
impl Network {
    /// The kind of a zebra network: regtest is zebra's testnet with regtest
    /// parameters.
    pub fn from_zebra(network: &zebra_chain::parameters::Network) -> Self {
        match network {
            zebra_chain::parameters::Network::Mainnet => Network::Mainnet,
            zebra_chain::parameters::Network::Testnet(parameters) => {
                if parameters.is_regtest() {
                    Network::Regtest
                } else {
                    Network::PubTestnet
                }
            }
        }
    }
}

/// Configurable activation heights for a Regtest validator launch.
///
/// Our own type rather than zebra's: this one is `Debug`, `PartialEq`, `Eq`
/// and `Copy`, carries our defaults, and keeps zebra behind the `zebra`
/// feature.
#[derive(Clone, Debug, PartialEq, Eq, Copy)]
#[cfg_attr(
    feature = "serde",
    derive(serde::Serialize, serde::Deserialize),
    serde(rename_all = "PascalCase", deny_unknown_fields)
)]
pub struct ActivationHeights {
    /// Activation height for `BeforeOverwinter` network upgrade.
    pub before_overwinter: Option<u32>,
    /// Activation height for `Overwinter` network upgrade.
    pub overwinter: Option<u32>,
    /// Activation height for `Sapling` network upgrade.
    pub sapling: Option<u32>,
    /// Activation height for `Blossom` network upgrade.
    pub blossom: Option<u32>,
    /// Activation height for `Heartwood` network upgrade.
    pub heartwood: Option<u32>,
    /// Activation height for `Canopy` network upgrade.
    pub canopy: Option<u32>,
    /// Activation height for `NU5` network upgrade.
    #[cfg_attr(feature = "serde", serde(rename = "NU5"))]
    pub nu5: Option<u32>,
    /// Activation height for `NU6` network upgrade.
    #[cfg_attr(feature = "serde", serde(rename = "NU6"))]
    pub nu6: Option<u32>,
    /// Activation height for `NU6.1` network upgrade.
    /// see <https://zips.z.cash/#nu6-1-candidate-zips> for info on NU6.1
    #[cfg_attr(feature = "serde", serde(rename = "NU6.1"))]
    pub nu6_1: Option<u32>,
    /// Activation height for `NU6.2` network upgrade.
    #[cfg_attr(feature = "serde", serde(rename = "NU6.2"))]
    pub nu6_2: Option<u32>,
    /// Activation height for `NU6.3` network upgrade.
    #[cfg_attr(feature = "serde", serde(rename = "NU6.3"))]
    pub nu6_3: Option<u32>,
    /// Activation height for `NU7` network upgrade.
    #[cfg_attr(feature = "serde", serde(rename = "NU7"))]
    pub nu7: Option<u32>,
}

impl Default for ActivationHeights {
    fn default() -> Self {
        ActivationHeights {
            before_overwinter: Some(1),
            overwinter: Some(1),
            sapling: Some(1),
            blossom: Some(1),
            heartwood: Some(1),
            canopy: Some(1),
            nu5: Some(2),
            nu6: Some(2),
            nu6_1: Some(2),
            nu6_2: Some(2),
            nu6_3: None,
            nu7: None,
        }
    }
}

/// Records the `NetworkUpgrade`-variant ↔ `ActivationHeights`-field
/// correspondence exactly once, generating everything derived from it: the
/// all-`None` [`ActivationHeights::NEVER_ACTIVATED`] schedule and, with the
/// `zebra` feature, the field-by-field [`ActivationHeights::to_zebra`]
/// conversion (the structs share field names) and the per-upgrade
/// [`ActivationHeights::slot_mut`] accessor.
///
/// A macro because plain `fn`s cannot abstract over struct fields, and the
/// variant/field spellings (`Nu5`/`nu5`) differ only by casing, which
/// `macro_rules!` cannot derive — hence explicit pairs. A new network upgrade
/// is one `(Variant, field)` entry below, after the struct field; the
/// exhaustive destructure and match fail the build until it is added.
macro_rules! activation_heights_mirror {
    ($(($variant:ident, $field:ident)),* $(,)?) => {
        impl ActivationHeights {
            /// The all-`None` schedule: every upgrade never-activated. The
            /// starting point for building heights from an external report,
            /// where an upgrade absent from the report must stay unset.
            pub const NEVER_ACTIVATED: Self = Self { $($field: None),* };
        }

        #[cfg(feature = "zebra")]
        impl ActivationHeights {
            /// These heights as zebra configures a regtest with them.
            pub fn to_zebra(self) -> zebra_chain::parameters::testnet::ConfiguredActivationHeights {
                let ActivationHeights { $($field),* } = self;
                zebra_chain::parameters::testnet::ConfiguredActivationHeights { $($field),* }
            }

            /// Mutable slot for `upgrade`'s activation height, or `None` for
            /// `Genesis` (height 0 by definition; it has no slot).
            pub fn slot_mut(
                &mut self,
                upgrade: zebra_chain::parameters::NetworkUpgrade,
            ) -> Option<&mut Option<u32>> {
                match upgrade {
                    zebra_chain::parameters::NetworkUpgrade::Genesis => None,
                    $(
                        zebra_chain::parameters::NetworkUpgrade::$variant => {
                            Some(&mut self.$field)
                        }
                    )*
                }
            }
        }
    };
}

activation_heights_mirror!(
    (BeforeOverwinter, before_overwinter),
    (Overwinter, overwinter),
    (Sapling, sapling),
    (Blossom, blossom),
    (Heartwood, heartwood),
    (Canopy, canopy),
    (Nu5, nu5),
    (Nu6, nu6),
    (Nu6_1, nu6_1),
    (Nu6_2, nu6_2),
    (Nu6_3, nu6_3),
    (Nu7, nu7),
);

#[cfg(feature = "zebra")]
impl ActivationHeights {
    /// Builds the runtime regtest network for a chain whose schedule is
    /// known first-hand: a validator being *launched* with these heights, or
    /// a test fixture that is its own chain (mockchain sources, proptest
    /// block generators). Production indexer code never calls this with
    /// configured values — it adopts the runtime network from the
    /// validator's reported schedule instead (zaino#1076).
    pub fn to_regtest_network(&self) -> zebra_chain::parameters::Network {
        zebra_chain::parameters::Network::new_regtest(self.to_zebra().into())
    }
}

#[cfg(all(test, feature = "zebra"))]
mod tests {
    use super::ActivationHeights;

    #[test]
    fn activation_heights_round_trip_nu6_2() {
        let heights = ActivationHeights {
            nu6_2: Some(2),
            nu6_3: Some(500),
            nu7: Some(1000),
            ..ActivationHeights::default()
        };

        assert_eq!(heights.to_zebra().nu6_2, Some(2));
    }
}
