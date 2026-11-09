//! One network-upgrade schedule: the blocks' transaction formats and branch
//! ids, which pools are active, and what `getblockchaininfo` reports.

use zaino_consensus::branch_id;
use zaino_primitives::types::{
    ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus, ShieldedPool,
};

/// A network upgrade the builder schedules.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Upgrade {
    /// Overwinter: v3 transactions.
    Overwinter,
    /// Sapling: v4 transactions, the Sapling pool.
    Sapling,
    /// Blossom.
    Blossom,
    /// Heartwood.
    Heartwood,
    /// Canopy.
    Canopy,
    /// NU5: v5 transactions, the Orchard pool.
    Nu5,
    /// NU6.
    Nu6,
    /// NU6.1.
    Nu6_1,
    /// NU6.2.
    Nu6_2,
    /// NU6.3: v6 transactions, the Ironwood pool.
    Nu6_3,
    /// NU7.
    Nu7,
}

impl Upgrade {
    /// Every upgrade, in activation order.
    pub const ALL: [Self; 11] = [
        Self::Overwinter,
        Self::Sapling,
        Self::Blossom,
        Self::Heartwood,
        Self::Canopy,
        Self::Nu5,
        Self::Nu6,
        Self::Nu6_1,
        Self::Nu6_2,
        Self::Nu6_3,
        Self::Nu7,
    ];

    /// Its consensus branch id.
    pub fn branch_id(self) -> u32 {
        match self {
            Self::Overwinter => branch_id::OVERWINTER,
            Self::Sapling => branch_id::SAPLING,
            Self::Blossom => branch_id::BLOSSOM,
            Self::Heartwood => branch_id::HEARTWOOD,
            Self::Canopy => branch_id::CANOPY,
            Self::Nu5 => branch_id::NU5,
            Self::Nu6 => branch_id::NU6,
            Self::Nu6_1 => branch_id::NU6_1,
            Self::Nu6_2 => branch_id::NU6_2,
            Self::Nu6_3 => branch_id::NU6_3,
            Self::Nu7 => branch_id::NU7,
        }
    }

    /// Its name as zcashd reports it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Overwinter => "Overwinter",
            Self::Sapling => "Sapling",
            Self::Blossom => "Blossom",
            Self::Heartwood => "Heartwood",
            Self::Canopy => "Canopy",
            Self::Nu5 => "NU5",
            Self::Nu6 => "NU6",
            Self::Nu6_1 => "NU6.1",
            Self::Nu6_2 => "NU6.2",
            Self::Nu6_3 => "NU6.3",
            Self::Nu7 => "NU7",
        }
    }

    /// The upgrade a pool activates with.
    pub fn of_pool(pool: ShieldedPool) -> Self {
        match pool {
            ShieldedPool::Sapling => Self::Sapling,
            ShieldedPool::Orchard => Self::Nu5,
            ShieldedPool::Ironwood => Self::Nu6_3,
        }
    }
}

/// Network-upgrade activation heights.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Activations {
    heights: Vec<(Upgrade, u32)>,
}

impl Default for Activations {
    /// Every upgrade through NU6.2 at height 1, as a regtest node runs:
    /// genesis is pre-Overwinter; Sapling and Orchard active, no Ironwood.
    fn default() -> Self {
        Self::through(Upgrade::Nu6_2, 1)
    }
}

impl Activations {
    /// No upgrade: every block pre-Overwinter.
    pub fn none() -> Self {
        Self {
            heights: Vec::new(),
        }
    }

    /// Every upgrade up to and including `last` at `height`.
    pub fn through(last: Upgrade, height: u32) -> Self {
        Self {
            heights: Upgrade::ALL
                .into_iter()
                .filter(|upgrade| *upgrade <= last)
                .map(|upgrade| (upgrade, height))
                .collect(),
        }
    }

    /// The same schedule with `upgrade` at `height`.
    pub fn at(mut self, upgrade: Upgrade, height: u32) -> Self {
        self.heights.retain(|(scheduled, _)| *scheduled != upgrade);
        self.heights.push((upgrade, height));
        self.heights.sort();
        self
    }

    /// The upgrade in force at `height` (`None` = pre-Overwinter): the latest
    /// activated at or below it.
    pub fn in_force(&self, height: u32) -> Option<Upgrade> {
        self.heights
            .iter()
            .filter(|(_, from)| *from <= height)
            .map(|(upgrade, _)| *upgrade)
            .max()
    }

    /// Whether `pool` is active at `height`.
    pub fn active(&self, pool: ShieldedPool, height: u32) -> bool {
        self.in_force(height)
            .is_some_and(|upgrade| upgrade >= Upgrade::of_pool(pool))
    }

    /// The schedule as a validator reports it (status set against the tip
    /// when answered).
    pub fn upgrades(&self) -> Vec<NetworkUpgradeInfo> {
        self.heights
            .iter()
            .filter_map(|(upgrade, height)| {
                Some(NetworkUpgradeInfo {
                    branch_id: ConsensusBranchId::new(upgrade.branch_id()),
                    name: upgrade.name().to_owned(),
                    activation_height: Height::try_from(*height).ok()?,
                    status: NetworkUpgradeStatus::Pending,
                })
            })
            .collect()
    }
}
