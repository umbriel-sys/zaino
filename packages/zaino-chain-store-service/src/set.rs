//! Which indexes a store builds (D-16, D-46, D-77): a named set, adjusted,
//! closed over its dependencies.

use std::collections::BTreeSet;

use zaino_indexes::IndexId;

/// The named sets (§5.4): what each serves, as index lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedSet {
    /// Compact blocks at scale, headers, treestates, sizes and subtree roots.
    Lightclient,
    /// The zcash JSON-RPC methods zaino serves locally; no compact blocks.
    ZcashRpc,
    /// Every index.
    Full,
    /// Nothing stored: every read goes to the validator.
    Empty,
}

impl NamedSet {
    /// Its indexes, before dependencies.
    pub fn indexes(self) -> &'static [IndexId] {
        use IndexId::*;
        match self {
            Self::Lightclient => &[
                Headers,
                Txids,
                TxidLocation,
                Transparent,
                CompactBlock,
                ChainMetadata,
                TreeState,
            ],
            Self::ZcashRpc => &[
                Headers,
                Txids,
                TxidLocation,
                Transparent,
                ChainMetadata,
                Spent,
                AddressHistory,
                TxOutSet,
                TreeState,
            ],
            Self::Full => IndexId::ALL,
            Self::Empty => &[],
        }
    }
}

/// Why a set cannot be formed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SetError {
    /// A name no index has.
    #[error("no index is named {0:?}")]
    UnknownIndex(String),
    /// A disabled index another enabled index depends on.
    #[error("{disabled} is disabled but {needed_by} depends on it")]
    DisabledDependency {
        /// The disabled index.
        disabled: &'static str,
        /// The enabled index that reads or builds from it.
        needed_by: &'static str,
    },
}

/// The enabled indexes, closed over [`IndexId::reads`] and
/// [`IndexId::builds_from`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexSet {
    ids: BTreeSet<IndexId>,
}

impl IndexSet {
    /// `base` (none = empty) plus `enable`, minus `disable`, closed over
    /// dependencies. Disabling an index something enabled depends on is
    /// refused rather than overridden.
    pub fn resolve(
        base: Option<NamedSet>,
        enable: &[String],
        disable: &[String],
    ) -> Result<Self, SetError> {
        let named = |names: &[String]| -> Result<BTreeSet<IndexId>, SetError> {
            names
                .iter()
                .map(|name| {
                    IndexId::from_name(name).ok_or_else(|| SetError::UnknownIndex(name.clone()))
                })
                .collect()
        };
        let (enable, disable) = (named(enable)?, named(disable)?);
        let mut ids: BTreeSet<IndexId> = base
            .map_or(&[][..], NamedSet::indexes)
            .iter()
            .copied()
            .collect();
        ids.extend(&enable);
        ids.retain(|id| !disable.contains(id));
        let mut pending: Vec<IndexId> = ids.iter().copied().collect();
        while let Some(id) = pending.pop() {
            for needed in dependencies(id) {
                if disable.contains(&needed) {
                    return Err(SetError::DisabledDependency {
                        disabled: needed.name(),
                        needed_by: id.name(),
                    });
                }
                if ids.insert(needed) {
                    pending.push(needed);
                }
            }
        }
        Ok(Self { ids })
    }

    /// Exactly `ids`, closed over dependencies.
    pub fn of(ids: impl IntoIterator<Item = IndexId>) -> Self {
        let mut set = Self::default();
        let mut pending: Vec<IndexId> = ids.into_iter().collect();
        while let Some(id) = pending.pop() {
            if set.ids.insert(id) {
                pending.extend(dependencies(id));
            }
        }
        set
    }

    /// Whether `id` is enabled.
    pub fn contains(&self, id: IndexId) -> bool {
        self.ids.contains(&id)
    }

    /// Whether nothing is enabled.
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The enabled indexes, each after every index it builds from.
    pub fn build_order(&self) -> Vec<IndexId> {
        let mut order: Vec<IndexId> = Vec::with_capacity(self.ids.len());
        let mut left: Vec<IndexId> = self.ids.iter().copied().collect();
        while !left.is_empty() {
            let before = left.len();
            left.retain(|id| {
                let ready = id.builds_from().all(|needed| order.contains(&needed));
                if ready {
                    order.push(*id);
                }
                !ready
            });
            // BUILDS_FROM is acyclic (the id tests pin it); never loop on a cycle
            if left.len() == before {
                order.append(&mut left);
            }
        }
        order
    }

    /// Whether an enabled index builds from others: the ingest must resolve
    /// prevouts.
    pub fn resolves_prevouts(&self) -> bool {
        self.ids.iter().any(|id| id.builds_from().next().is_some())
    }

    /// The enabled indexes, ascending.
    pub fn iter(&self) -> impl Iterator<Item = IndexId> + '_ {
        self.ids.iter().copied()
    }
}

fn dependencies(id: IndexId) -> impl Iterator<Item = IndexId> {
    id.reads().chain(id.builds_from())
}

#[cfg(test)]
mod tests {
    use zaino_indexes::IndexId::{self, *};

    use super::{IndexSet, NamedSet, SetError};

    fn names(ids: &[IndexId]) -> Vec<String> {
        ids.iter().map(|id| id.name().to_owned()).collect()
    }

    #[test]
    fn named_sets_are_closed_already() {
        for set in [
            NamedSet::Lightclient,
            NamedSet::ZcashRpc,
            NamedSet::Full,
            NamedSet::Empty,
        ] {
            let resolved = IndexSet::resolve(Some(set), &[], &[]).expect("resolves");
            assert_eq!(
                resolved.iter().collect::<Vec<_>>(),
                {
                    let mut ids = set.indexes().to_vec();
                    ids.sort();
                    ids
                },
                "{set:?} lists its dependencies"
            );
        }
    }

    #[test]
    fn enabling_pulls_in_dependencies() {
        let set = IndexSet::resolve(None, &names(&[TxOutSet]), &[]).expect("resolves");
        // I-11 builds from I-03 and I-04; I-03 reads I-02
        assert_eq!(
            set.iter().collect::<Vec<_>>(),
            [Txids, TxidLocation, Transparent, TxOutSet]
        );
        assert!(set.resolves_prevouts());
        let plain = IndexSet::resolve(None, &names(&[Spent]), &[]).expect("resolves");
        assert_eq!(plain.iter().collect::<Vec<_>>(), [Txids, Spent]);
        assert!(!plain.resolves_prevouts());
    }

    #[test]
    fn disabling_a_needed_index_or_naming_none_is_refused() {
        assert!(matches!(
            IndexSet::resolve(Some(NamedSet::Full), &[], &names(&[Txids])),
            Err(SetError::DisabledDependency { disabled, .. }) if disabled == Txids.name()
        ));
        assert_eq!(
            IndexSet::resolve(None, &["nope".to_owned()], &[]),
            Err(SetError::UnknownIndex("nope".to_owned()))
        );
        let without = IndexSet::resolve(Some(NamedSet::Full), &[], &names(&[HistoryTree]))
            .expect("nothing needs C6");
        assert!(!without.contains(HistoryTree));
    }

    #[test]
    fn the_build_order_puts_sources_first() {
        let order = IndexSet::resolve(Some(NamedSet::Full), &[], &[])
            .expect("resolves")
            .build_order();
        assert_eq!(order.len(), IndexId::ALL.len());
        for (at, id) in order.iter().enumerate() {
            for needed in id.builds_from() {
                let before = order
                    .iter()
                    .position(|other| *other == needed)
                    .expect("enabled");
                assert!(before < at, "{} before {}", needed.name(), id.name());
            }
        }
    }
}
