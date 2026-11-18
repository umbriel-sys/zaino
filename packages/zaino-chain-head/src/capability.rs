//! What a snapshot builds beyond the graph (D-56): the mirror of the store's
//! capability set, so a consumer routes per capability at runtime.

use core::fmt;

/// A capability an extension gives a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum HeadCapability {
    /// Transaction positions and outpoint spenders inside the window.
    Transactions,
    /// The UTXO set at the tip.
    TxOutSet,
    /// Transparent address effects inside the window.
    TransparentHistory,
    /// The note commitment trees after each block in the window.
    TreeState,
    /// The history tree after each block in the window.
    HistoryTree,
}

impl HeadCapability {
    /// Every capability, in a fixed order.
    pub const ALL: [Self; 5] = [
        Self::Transactions,
        Self::TxOutSet,
        Self::TransparentHistory,
        Self::TreeState,
        Self::HistoryTree,
    ];

    const fn bit(self) -> u8 {
        match self {
            Self::Transactions => 1,
            Self::TxOutSet => 1 << 1,
            Self::TransparentHistory => 1 << 2,
            Self::TreeState => 1 << 3,
            Self::HistoryTree => 1 << 4,
        }
    }
}

impl fmt::Display for HeadCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transactions => "transactions",
            Self::TxOutSet => "txout set",
            Self::TransparentHistory => "transparent history",
            Self::TreeState => "tree state",
            Self::HistoryTree => "history tree",
        })
    }
}

/// The capabilities a snapshot's built extensions give it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeadCapabilities(u8);

impl HeadCapabilities {
    /// No extension built.
    pub const NONE: Self = Self(0);

    /// `self` with `capability`.
    #[must_use]
    pub const fn with(self, capability: HeadCapability) -> Self {
        Self(self.0 | capability.bit())
    }

    /// Whether `capability` is built.
    pub const fn contains(self, capability: HeadCapability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// The built capabilities, in [`HeadCapability::ALL`] order.
    pub fn iter(self) -> impl Iterator<Item = HeadCapability> {
        HeadCapability::ALL
            .into_iter()
            .filter(move |capability| self.contains(*capability))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_are_a_set() {
        let built = HeadCapabilities::NONE
            .with(HeadCapability::TxOutSet)
            .with(HeadCapability::TreeState)
            .with(HeadCapability::TxOutSet);
        assert!(built.contains(HeadCapability::TxOutSet));
        assert!(!built.contains(HeadCapability::Transactions));
        assert_eq!(
            built.iter().collect::<Vec<_>>(),
            vec![HeadCapability::TxOutSet, HeadCapability::TreeState]
        );
        assert_eq!(HeadCapabilities::NONE.iter().count(), 0);
    }
}
