//! Integrity and size reports a [`Backend`](crate::Backend) answers.

use zaino_primitives::types::BlockRef;

use crate::backend::Namespace;

/// What [`Backend::verify`](crate::Backend::verify) found: the checked commit's
/// tip and size, and one unit per stored file (or the backend's equivalent);
/// no unit when there is nothing to check.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Verification {
    /// The tip the checked commit was made at; `None` = nothing committed.
    #[cfg_attr(feature = "serde", serde(serialize_with = "serialize_tip"))]
    pub tip: Option<BlockRef>,
    /// Bytes the checked commit holds across every unit.
    pub committed_bytes: u64,
    /// Each unit checked.
    pub units: Vec<Unit>,
}

impl Verification {
    /// No unit damaged ([`Problem::Orphaned`] is harmless).
    pub fn is_clean(&self) -> bool {
        self.units
            .iter()
            .flat_map(|unit| &unit.problems)
            .all(|problem| !problem.is_damage())
    }
}

/// `{ height, hash }` with the hash in display order, as the RPCs render one.
#[cfg(feature = "serde")]
fn serialize_tip<S: serde::Serializer>(
    tip: &Option<BlockRef>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize as _;

    #[derive(serde::Serialize)]
    struct Tip {
        height: u32,
        hash: String,
    }
    tip.map(|tip| Tip {
        height: u32::from(tip.height),
        hash: tip.hash.to_string(),
    })
    .serialize(serializer)
}

/// One stored unit and what its check found.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct Unit {
    /// The unit's name (e.g. a file path under the backend's directory).
    pub name: String,
    /// What is wrong with it; empty = clean.
    pub problems: Vec<Problem>,
}

/// A finding against a stored unit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub enum Problem {
    /// The unit is gone.
    Missing,
    /// Shorter than committed.
    Truncated,
    /// A page's bytes differ from its checksum.
    ChecksumMismatch {
        /// Page index.
        page: u64,
    },
    /// The unit's checksum list differs from what the commit recorded.
    ChecksumListMismatch,
    /// Bytes past what the commit claims (an interrupted commit's tail: harmless).
    Orphaned {
        /// Bytes past the committed length.
        bytes: u64,
    },
}

impl Problem {
    /// Whether committed data is affected.
    pub fn is_damage(&self) -> bool {
        !matches!(self, Self::Orphaned { .. })
    }
}

/// What one namespace occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct NamespaceFootprint {
    /// The namespace.
    pub namespace: Namespace,
    /// Bytes stored.
    pub bytes: u64,
    /// Of `bytes`, those in the page cache; `None` = not knowable.
    pub cached: Option<u64>,
}

#[cfg(all(test, feature = "serde"))]
mod tests {
    use zaino_primitives::types::{BlockHash, BlockRef, Height};

    use super::{Problem, Unit, Verification};

    #[test]
    fn a_report_serialises_its_tip_in_display_order() {
        let report = Verification {
            tip: Some(BlockRef {
                hash: BlockHash::from([1; 32]),
                height: Height::try_from(7).expect("in range"),
            }),
            committed_bytes: 42,
            units: vec![Unit {
                name: "blocks.dat".to_owned(),
                problems: vec![Problem::Orphaned { bytes: 3 }],
            }],
        };
        let json = serde_json::to_value(&report).expect("serialises");
        assert_eq!(json["tip"]["height"], 7);
        assert_eq!(json["tip"]["hash"], "01".repeat(32));
        assert_eq!(json["committed_bytes"], 42);
        assert_eq!(json["units"][0]["problems"][0]["Orphaned"]["bytes"], 3);
        let empty = serde_json::to_value(Verification::default()).expect("serialises");
        assert!(empty["tip"].is_null());
    }
}
