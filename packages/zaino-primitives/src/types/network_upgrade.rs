//! Network upgrade schedule as reported by the validator.
//!
//! Zaino does not carry a compiled-in activation schedule for the chain it
//! serves — it adopts the one the validator reports, so an indexer and its
//! validator cannot disagree about where an upgrade activates.

use super::Height;

/// A consensus branch identifier.
///
/// The protocol-defined identity of a network upgrade, and the stable key for
/// one: unlike a name, it is fixed by consensus and cannot be spelled two ways.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConsensusBranchId(u32);

impl ConsensusBranchId {
    /// Wrap a raw branch identifier.
    pub const fn new(id: u32) -> Self {
        Self(id)
    }
}

impl From<u32> for ConsensusBranchId {
    fn from(id: u32) -> Self {
        Self(id)
    }
}

impl From<ConsensusBranchId> for u32 {
    fn from(id: ConsensusBranchId) -> Self {
        id.0
    }
}

impl core::fmt::Display for ConsensusBranchId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Branch IDs are conventionally written as 8-digit lowercase hex.
        write!(f, "{:08x}", self.0)
    }
}

/// How far along a network upgrade is at the validator's current tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkUpgradeStatus {
    /// Activated. Includes upgrades activated long ago, not just the latest.
    Active,
    /// Has an activation height that the chain has not reached yet.
    Pending,
    /// Has no activation height on this network.
    Disabled,
}

/// One entry in the validator's network upgrade schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetworkUpgradeInfo {
    /// Consensus branch identifier of this upgrade.
    pub branch_id: ConsensusBranchId,

    /// The validator's name for the upgrade, e.g. `"Canopy"`, `"NU5"`.
    ///
    /// Descriptive only — never match on it. This is a `String` rather than an
    /// enum on purpose: the set of upgrades grows with the protocol, so an enum
    /// here could not represent an upgrade released after this crate was built,
    /// and a validator ahead of Zaino is exactly the case that must not fail.
    /// [`Self::branch_id`] is the identity to key on.
    pub name: String,

    /// Height at which the upgrade activates.
    pub activation_height: Height,

    /// Status at the validator's current tip.
    pub status: NetworkUpgradeStatus,
}

/// The consensus branch in force at `height` under the validator's
/// `schedule`: the upgrade with the highest activation height at or below
/// it, ignoring disabled ones. `None` before the first branch-carrying
/// upgrade (Overwinter). Every historic block gets the branch its own
/// height was mined under.
pub fn branch_at(schedule: &[NetworkUpgradeInfo], height: Height) -> Option<ConsensusBranchId> {
    schedule
        .iter()
        .filter(|upgrade| upgrade.status != NetworkUpgradeStatus::Disabled)
        .filter(|upgrade| upgrade.activation_height <= height)
        .max_by_key(|upgrade| upgrade.activation_height)
        .map(|upgrade| upgrade.branch_id)
}

/// The consensus branches in force around the current tip.
///
/// The two differ exactly when the next block activates a network upgrade,
/// which is what makes this worth reporting as a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConsensusBranchIds {
    /// Branch in force at the current tip.
    pub chain_tip: ConsensusBranchId,
    /// Branch that will be in force for the next block.
    pub next_block: ConsensusBranchId,
}

impl ConsensusBranchIds {
    /// Whether the next block activates a network upgrade.
    pub fn next_block_activates_upgrade(&self) -> bool {
        self.chain_tip != self.next_block
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn branch_id_displays_as_eight_digit_hex() {
        assert_eq!(ConsensusBranchId::new(0xc2d6_d0b4).to_string(), "c2d6d0b4");
        assert_eq!(ConsensusBranchId::new(0).to_string(), "00000000");
    }

    /// Each height gets the branch activated at or below it, including on
    /// the activation height itself; a disabled upgrade never applies.
    #[test]
    fn historic_heights_get_their_own_branch() {
        let info = |branch: u32, height: u32, status| NetworkUpgradeInfo {
            branch_id: ConsensusBranchId::new(branch),
            name: String::new(),
            activation_height: Height::try_from(height).expect("height"),
            status,
        };
        // Out of order on purpose: the schedule is a set, not a sequence.
        let schedule = [
            info(3, 300, NetworkUpgradeStatus::Active),
            info(1, 100, NetworkUpgradeStatus::Active),
            info(2, 200, NetworkUpgradeStatus::Active),
            info(4, 400, NetworkUpgradeStatus::Pending),
            info(9, 250, NetworkUpgradeStatus::Disabled),
        ];
        let at = |height: u32| {
            branch_at(&schedule, Height::try_from(height).expect("height")).map(u32::from)
        };
        assert_eq!(at(0), None);
        assert_eq!(at(99), None);
        assert_eq!(at(100), Some(1));
        assert_eq!(at(199), Some(1));
        assert_eq!(at(200), Some(2));
        assert_eq!(at(260), Some(2), "a disabled upgrade never applies");
        assert_eq!(at(300), Some(3));
        assert_eq!(at(400), Some(4), "a pending upgrade applies once reached");
    }

    #[test]
    fn upgrade_activation_is_a_branch_change() {
        let steady = ConsensusBranchIds {
            chain_tip: ConsensusBranchId::new(1),
            next_block: ConsensusBranchId::new(1),
        };
        let activating = ConsensusBranchIds {
            chain_tip: ConsensusBranchId::new(1),
            next_block: ConsensusBranchId::new(2),
        };

        assert!(!steady.next_block_activates_upgrade());
        assert!(activating.next_block_activates_upgrade());
    }
}
