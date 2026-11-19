//! The network upgrade schedule, read against a tip (`GetLightdInfo`).
//!
//! ```text
//! branch_in_force(tip) = branch of max { u ∈ upgrades | activation(u) ≤ tip }
//! next_upgrade(tip)    = min { u ∈ upgrades | activation(u) > tip }
//! sapling_activation   = activation of the upgrade carrying Sapling's branch id
//! ```
//!
//! Upgrades the validator marks disabled are skipped.

use zaino_primitives::types::{
    ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus,
};

/// The schedule's facts at one tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduleAt<'a> {
    /// Where Sapling activates, if scheduled.
    pub sapling_activation: Option<Height>,
    /// The consensus branch in force at the tip.
    pub branch_in_force: Option<ConsensusBranchId>,
    /// The first upgrade above the tip.
    pub next_upgrade: Option<&'a NetworkUpgradeInfo>,
}

/// `upgrades` read against `tip`.
pub fn schedule_at(upgrades: &[NetworkUpgradeInfo], tip: Height) -> ScheduleAt<'_> {
    let scheduled = || {
        upgrades
            .iter()
            .filter(|upgrade| upgrade.status != NetworkUpgradeStatus::Disabled)
    };
    ScheduleAt {
        sapling_activation: scheduled()
            .find(|upgrade| u32::from(upgrade.branch_id) == zaino_consensus::branch_id::SAPLING)
            .map(|upgrade| upgrade.activation_height),
        branch_in_force: scheduled()
            .filter(|upgrade| upgrade.activation_height <= tip)
            .max_by_key(|upgrade| upgrade.activation_height)
            .map(|upgrade| upgrade.branch_id),
        next_upgrade: scheduled()
            .filter(|upgrade| upgrade.activation_height > tip)
            .min_by_key(|upgrade| upgrade.activation_height),
    }
}

#[cfg(test)]
mod tests {
    use super::schedule_at;
    use zaino_consensus::branch_id::{NU5, NU6, SAPLING};
    use zaino_primitives::types::{
        ConsensusBranchId, Height, NetworkUpgradeInfo, NetworkUpgradeStatus,
    };

    fn h(height: u32) -> Height {
        Height::try_from(height).expect("a valid test height")
    }

    fn upgrade(branch: u32, activation: u32, status: NetworkUpgradeStatus) -> NetworkUpgradeInfo {
        NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch),
            name: format!("{branch:08x}"),
            activation_height: h(activation),
            status,
        }
    }

    fn mainnet_like() -> Vec<NetworkUpgradeInfo> {
        vec![
            upgrade(NU6, 2_726_400, NetworkUpgradeStatus::Pending),
            upgrade(SAPLING, 419_200, NetworkUpgradeStatus::Active),
            upgrade(NU5, 1_687_104, NetworkUpgradeStatus::Active),
        ]
    }

    /// Between two upgrades the earlier is in force and the later is next,
    /// whatever order the validator listed them in.
    #[test]
    fn between_upgrades_the_earlier_is_in_force_and_the_later_is_next() {
        let upgrades = mainnet_like();
        let schedule = schedule_at(&upgrades, h(2_000_000));
        assert_eq!(schedule.sapling_activation, Some(h(419_200)));
        assert_eq!(schedule.branch_in_force, Some(ConsensusBranchId::new(NU5)));
        assert_eq!(
            schedule.next_upgrade.map(|upgrade| upgrade.branch_id),
            Some(ConsensusBranchId::new(NU6))
        );
    }

    /// An upgrade is in force from its activation height itself.
    #[test]
    fn an_upgrade_is_in_force_at_its_activation_height() {
        let upgrades = mainnet_like();
        let schedule = schedule_at(&upgrades, h(2_726_400));
        assert_eq!(schedule.branch_in_force, Some(ConsensusBranchId::new(NU6)));
        assert!(schedule.next_upgrade.is_none());
    }

    /// A disabled upgrade is neither in force, nor next, nor Sapling's.
    #[test]
    fn a_disabled_upgrade_is_skipped() {
        let upgrades = vec![
            upgrade(SAPLING, 1, NetworkUpgradeStatus::Disabled),
            upgrade(NU5, 10, NetworkUpgradeStatus::Disabled),
        ];
        let schedule = schedule_at(&upgrades, h(5));
        assert_eq!(schedule.sapling_activation, None);
        assert_eq!(schedule.branch_in_force, None);
        assert!(schedule.next_upgrade.is_none());
    }

    /// No Sapling upgrade is no activation height, never a made-up one (K-02).
    #[test]
    fn a_missing_sapling_upgrade_is_none() {
        let upgrades = vec![upgrade(NU5, 10, NetworkUpgradeStatus::Active)];
        assert_eq!(schedule_at(&upgrades, h(20)).sapling_activation, None);
    }
}
