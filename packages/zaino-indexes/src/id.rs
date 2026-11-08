//! Every index as a runtime value (D-76): a closed enum dispatching to each
//! index's [`Index`] impl, and the enum of their fold states.

use zaino_persistence::BackendReader;

use crate::block::FoldBlock;
use crate::index::{Fold, FoldError, Index};
use crate::schema::Schema;

/// Declares [`IndexId`] and [`IndexState`] over each `Variant => module::Type`.
/// A macro because the enums' variants are generated from the list.
macro_rules! indexes {
    ($($(#[$doc:meta])* $variant:ident => $module:ident::$index:ident),* $(,)?) => {
        /// One index, by identity.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum IndexId {
            $($(#[$doc])* $variant,)*
        }

        /// One index's fold state, tagged with its index.
        #[derive(Debug, Clone)]
        pub enum IndexState {
            $(
                #[doc = concat!("[`IndexId::", stringify!($variant), "`]'s state.")]
                $variant(<crate::$module::$index as Index>::State),
            )*
        }

        impl IndexId {
            /// Every index, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant,)*];

            /// Its disk schema.
            pub const fn schema(self) -> Schema {
                match self {
                    $(Self::$variant => <crate::$module::$index as Index>::SCHEMA,)*
                }
            }

            const fn read_names(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => <crate::$module::$index as Index>::READS,)*
                }
            }

            const fn build_names(self) -> &'static [&'static str] {
                match self {
                    $(Self::$variant => <crate::$module::$index as Index>::BUILDS_FROM,)*
                }
            }

            /// The state after the last block `backend` holds.
            pub fn resume<R: BackendReader>(
                self,
                backend: &R,
            ) -> Result<Option<IndexState>, FoldError> {
                match self {
                    $(Self::$variant => Ok(
                        <crate::$module::$index as Index>::resume(backend)?.map(IndexState::$variant)
                    ),)*
                }
            }

            /// `block`'s ops and the state after them; `state` must be this
            /// index's.
            pub fn fold(
                self,
                state: Option<&IndexState>,
                block: &FoldBlock,
            ) -> Result<Fold<IndexState>, FoldError> {
                match (self, state) {
                    $(
                        (Self::$variant, None) => {
                            let fold = <crate::$module::$index as Index>::fold(None, block)?;
                            Ok(Fold { ops: fold.ops, next: IndexState::$variant(fold.next) })
                        }
                        (Self::$variant, Some(IndexState::$variant(state))) => {
                            let fold = <crate::$module::$index as Index>::fold(Some(state), block)?;
                            Ok(Fold { ops: fold.ops, next: IndexState::$variant(fold.next) })
                        }
                    )*
                    (id, Some(state)) => Err(FoldError::continuity(
                        id.name(),
                        format!("given {}'s state", state.id().name()),
                    )),
                }
            }
        }

        impl IndexState {
            /// The index this is the state of.
            pub const fn id(&self) -> IndexId {
                match self {
                    $(Self::$variant(_) => IndexId::$variant,)*
                }
            }
        }
    };
}

indexes! {
    /// I-01: headers, hashes and chain work by height.
    Headers => headers::Headers,
    /// I-02: each block's txids.
    Txids => txids::Txids,
    /// I-03: where each txid was mined.
    TxidLocation => txid_location::TxidLocation,
    /// I-04: transparent inputs and outputs.
    Transparent => transparent::Transparent,
    /// I-05: compact block columns.
    CompactBlock => compact_block::CompactBlock,
    /// I-08: commitment tree roots and sizes.
    ChainMetadata => chain_metadata::ChainMetadata,
    /// I-09: who spent each outpoint.
    Spent => spent::Spent,
    /// I-10: transparent address history.
    AddressHistory => address_history::AddressHistory,
    /// I-11: the UTXO set and its commitment.
    TxOutSet => tx_out_set::TxOutSet,
    /// C2: note commitment tree states and subtree roots.
    TreeState => tree_state::TreeState,
    /// C6: the history tree.
    HistoryTree => history_tree::HistoryTree,
}

impl IndexId {
    /// Its name: directory and config vocabulary.
    pub const fn name(self) -> &'static str {
        self.schema().name
    }

    /// The index named `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|id| id.name() == name)
    }

    /// The indexes its answers join ([`Index::READS`]).
    pub fn reads(self) -> impl Iterator<Item = Self> {
        ids(self.read_names())
    }

    /// The indexes the ingest reads to build it ([`Index::BUILDS_FROM`]).
    pub fn builds_from(self) -> impl Iterator<Item = Self> {
        ids(self.build_names())
    }
}

/// The indexes `names` name; the tests pin that every name is one.
fn ids(names: &'static [&'static str]) -> impl Iterator<Item = IndexId> {
    names.iter().filter_map(|name| IndexId::from_name(name))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::IndexId;

    #[test]
    fn names_are_unique_and_round_trip() {
        let names: HashSet<_> = IndexId::ALL.iter().map(|id| id.name()).collect();
        assert_eq!(names.len(), IndexId::ALL.len());
        for &id in IndexId::ALL {
            assert_eq!(IndexId::from_name(id.name()), Some(id));
        }
        assert_eq!(IndexId::from_name("nope"), None);
    }

    #[test]
    fn every_dependency_names_an_index() {
        for &id in IndexId::ALL {
            for name in id.read_names().iter().chain(id.build_names()) {
                assert!(IndexId::from_name(name).is_some(), "{}: {name}", id.name());
                assert_ne!(*name, id.name(), "{} depends on itself", id.name());
            }
        }
    }

    #[test]
    fn prevout_readers_build_from_i03_and_i04() {
        let prevouts = [IndexId::TxidLocation, IndexId::Transparent];
        for &id in IndexId::ALL {
            let builds: Vec<_> = id.builds_from().collect();
            let expected: &[IndexId] = match id {
                IndexId::CompactBlock | IndexId::AddressHistory | IndexId::TxOutSet => &prevouts,
                _ => &[],
            };
            assert_eq!(builds, expected, "{}", id.name());
        }
    }
}
