//! What the service answers, and from where (D6-12).

use zaino_chain::{Answerable, ChainCapability, ServiceabilityManifest};
use zaino_chain_store::StoreCapability;
use zaino_chain_store_service::{indexes_of, Serviceability};
use zaino_indexes::IndexId;
use zaino_primitives::types::Height;

use crate::error::ServiceError;

/// A capability the service answers: a chain read, or one above the chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Capability {
    /// A ChainView read.
    Chain(ChainCapability),
    /// The mempool: listings, transactions, the coherent stream.
    Mempool,
    /// Submitting a transaction.
    Broadcast,
    /// The validator's own status: chain, node, peer and mining info, fees.
    NodeStatus,
}

impl Capability {
    /// Every capability: ChainView's, then those above the chain.
    pub fn all() -> impl Iterator<Item = Self> {
        ChainCapability::ALL.into_iter().map(Self::Chain).chain([
            Self::Mempool,
            Self::Broadcast,
            Self::NodeStatus,
        ])
    }
}

impl core::fmt::Display for Capability {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Chain(capability) => capability.fmt(f),
            Self::Mempool => f.write_str("the mempool"),
            Self::Broadcast => f.write_str("broadcast"),
            Self::NodeStatus => f.write_str("node status"),
        }
    }
}

/// Who answers a capability now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provision {
    /// Zaino's own data: the index named (`None` for the chain head's window
    /// or the mempool, which no index backs).
    Local(Option<IndexId>),
    /// The validator.
    Validator,
    /// The index named is still building, at the height given (`None`:
    /// nothing yet); its reads are served as far as it reaches.
    Building {
        /// The first index of the capability not yet built.
        index: IndexId,
        /// How far it has built.
        height: Option<Height>,
    },
    /// Not served: the index named would serve it (`None`: no index can).
    Unavailable(Option<IndexId>),
}

/// Every capability with its provision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceManifest {
    entries: Vec<(Capability, Provision)>,
}

impl ServiceManifest {
    /// The manifest from ChainView's and the store's serviceability.
    pub(crate) fn derive(
        chain: &ServiceabilityManifest,
        store: impl Fn(IndexId) -> Serviceability,
    ) -> Self {
        Self {
            entries: Capability::all()
                .map(|capability| (capability, provision(capability, chain, &store)))
                .collect(),
        }
    }

    /// The provision of `capability`.
    pub fn get(&self, capability: Capability) -> Provision {
        self.entries
            .iter()
            .find(|(candidate, _)| *candidate == capability)
            .map_or(Provision::Unavailable(None), |(_, provision)| *provision)
    }

    /// Every capability and its provision.
    pub fn iter(&self) -> impl Iterator<Item = (Capability, Provision)> + '_ {
        self.entries.iter().copied()
    }

    /// `Ok` when every capability is served (locally, by the validator, or
    /// building); else the first that is not, naming the index to enable.
    pub fn require(&self, capabilities: &[Capability]) -> Result<(), ServiceError> {
        capabilities
            .iter()
            .try_for_each(|capability| match self.get(*capability) {
                Provision::Unavailable(index) => Err(ServiceError::NotServed {
                    capability: *capability,
                    index,
                }),
                Provision::Local(_) | Provision::Validator | Provision::Building { .. } => Ok(()),
            })
    }
}

/// The store capabilities whose indexes back a chain capability; `None` for
/// one this crate does not know.
fn backing(capability: ChainCapability) -> Option<&'static [StoreCapability]> {
    Some(match capability {
        ChainCapability::Blocks => &[StoreCapability::CompactBlocks, StoreCapability::TreeRoots],
        ChainCapability::CompactBlocks => &[StoreCapability::CompactBlocks],
        ChainCapability::Treestate | ChainCapability::SubtreeRoots => &[StoreCapability::TreeState],
        ChainCapability::AddressHistory => &[StoreCapability::TransparentHistory],
        ChainCapability::SpendStatus | ChainCapability::Prevouts => {
            &[StoreCapability::SpentOutputs]
        }
        ChainCapability::TxOutSet => &[StoreCapability::TxOutSet],
        ChainCapability::HistoryTree => &[StoreCapability::HistoryTree],
        ChainCapability::Transactions | ChainCapability::ChainTips => &[],
        _ => return None,
    })
}

fn provision(
    capability: Capability,
    chain: &ServiceabilityManifest,
    store: &impl Fn(IndexId) -> Serviceability,
) -> Provision {
    let capability = match capability {
        Capability::Chain(capability) => capability,
        Capability::Mempool => return Provision::Local(None),
        Capability::Broadcast | Capability::NodeStatus => return Provision::Validator,
    };
    let Some(backing) = backing(capability) else {
        return Provision::Unavailable(None);
    };
    let mut indexes: Vec<IndexId> = Vec::new();
    for id in backing.iter().flat_map(|backing| indexes_of(*backing)) {
        if !indexes.contains(&id) {
            indexes.push(id);
        }
    }
    let answerable = match chain.get(capability) {
        Answerable::Absent => return Provision::Unavailable(indexes.first().copied()),
        Answerable::ToHeight(_) => true,
        Answerable::NotAnswerable => false,
        _ => return Provision::Unavailable(indexes.first().copied()),
    };
    let Some(first) = indexes.first().copied() else {
        return match capability {
            ChainCapability::ChainTips => Provision::Local(None),
            _ => Provision::Validator,
        };
    };
    let states: Vec<(IndexId, Serviceability)> =
        indexes.iter().map(|id| (*id, store(*id))).collect();
    if states
        .iter()
        .any(|(_, state)| *state == Serviceability::Absent)
    {
        return Provision::Validator;
    }
    if let Some((index, height)) = states.iter().find_map(|(id, state)| match state {
        Serviceability::Building(height) => Some((*id, *height)),
        Serviceability::Absent | Serviceability::ToHeight(_) => None,
    }) {
        return Provision::Building { index, height };
    }
    match answerable {
        true => Provision::Local(Some(first)),
        // Every index built, but not yet answerable (the head's half, or a
        // hole): catching up at the store's height.
        false => Provision::Building {
            index: first,
            height: states.first().and_then(|(_, state)| match state {
                Serviceability::ToHeight(height) => Some(*height),
                Serviceability::Building(height) => *height,
                Serviceability::Absent => None,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> Height {
        Height::try_from(h).expect("small")
    }

    fn chain(answer: Answerable) -> ServiceabilityManifest {
        ServiceabilityManifest::derive(|_| answer)
    }

    fn compact_block() -> IndexId {
        indexes_of(StoreCapability::CompactBlocks)
            .next()
            .expect("a root")
    }

    /// Every capability appears once.
    #[test]
    fn every_capability_is_listed_once() {
        for capability in Capability::all() {
            assert_eq!(
                Capability::all()
                    .filter(|candidate| *candidate == capability)
                    .count(),
                1,
                "{capability}"
            );
        }
    }

    /// Built indexes are local, building ones report their height, absent
    /// ones fall to the validator while ChainView answers, and a capability
    /// ChainView cannot answer names the index that would.
    #[test]
    fn provisions_follow_the_store_and_chain_view() {
        let compact = Capability::Chain(ChainCapability::CompactBlocks);
        let built = ServiceManifest::derive(&chain(Answerable::ToHeight(height(9))), |_| {
            Serviceability::ToHeight(height(9))
        });
        assert_eq!(built.get(compact), Provision::Local(Some(compact_block())));
        let building = ServiceManifest::derive(&chain(Answerable::ToHeight(height(9))), |_| {
            Serviceability::Building(Some(height(4)))
        });
        assert_eq!(
            building.get(compact),
            Provision::Building {
                index: compact_block(),
                height: Some(height(4))
            }
        );
        let passthrough = ServiceManifest::derive(&chain(Answerable::ToHeight(height(9))), |_| {
            Serviceability::Absent
        });
        assert_eq!(passthrough.get(compact), Provision::Validator);
        let absent =
            ServiceManifest::derive(&chain(Answerable::Absent), |_| Serviceability::Absent);
        assert_eq!(
            absent.get(compact),
            Provision::Unavailable(Some(compact_block()))
        );
        assert!(absent.require(&[Capability::Mempool]).is_ok());
        assert!(matches!(
            absent.require(&[Capability::Mempool, compact]),
            Err(ServiceError::NotServed { capability, index })
                if capability == compact && index == Some(compact_block())
        ));
    }

    /// Indexes built but not answerable (the head's half missing) are
    /// building at the store's height; the window and the mempool are local;
    /// broadcast and node status are the validator's.
    #[test]
    fn the_rest_of_the_table() {
        let manifest = ServiceManifest::derive(&chain(Answerable::NotAnswerable), |_| {
            Serviceability::ToHeight(height(7))
        });
        let txout_set = indexes_of(StoreCapability::TxOutSet)
            .next()
            .expect("a root");
        assert_eq!(
            manifest.get(Capability::Chain(ChainCapability::TxOutSet)),
            Provision::Building {
                index: txout_set,
                height: Some(height(7))
            }
        );
        assert_eq!(
            manifest.get(Capability::Chain(ChainCapability::ChainTips)),
            Provision::Local(None)
        );
        assert_eq!(
            manifest.get(Capability::Chain(ChainCapability::Transactions)),
            Provision::Validator
        );
        assert_eq!(manifest.get(Capability::Mempool), Provision::Local(None));
        assert_eq!(manifest.get(Capability::Broadcast), Provision::Validator);
        assert_eq!(manifest.get(Capability::NodeStatus), Provision::Validator);
        assert!(manifest
            .require(&Capability::all().collect::<Vec<_>>())
            .is_ok());
    }
}
