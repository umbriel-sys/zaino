//! The chain index's legacy type vocabulary, and its conversions to and from
//! the domain types ChainView and the mempool speak.
//!
//! The legacy shapes live in `zaino-chain-store-zainodb` and are re-exported
//! because the public `ChainIndex` traits are written against them. Every
//! conversion between them and the domain is here, so both services convert
//! the same way.
//!
//! The wire conversions stay in [`super::wire_types`].

use std::collections::HashSet;
use std::num::NonZeroU128;

use zaino_chain::{ChainBlock, ChainTxPosition, SpendStatus};
use zaino_primitives::types as domain;
use zaino_service::Placement;

use crate::error::ChainIndexError;

pub use zaino_chain_store_zainodb::types::*;

pub(crate) fn domain_hash(hash: BlockHash) -> domain::BlockHash {
    domain::BlockHash::from(hash.0)
}

pub(crate) fn local_hash(hash: domain::BlockHash) -> BlockHash {
    BlockHash(hash.into())
}

/// `None` beyond the protocol maximum, where no block can be.
pub(crate) fn domain_height(height: Height) -> Option<domain::Height> {
    domain::Height::try_from(height.0).ok()
}

pub(crate) fn local_height(height: domain::Height) -> Height {
    Height(u32::from(height))
}

pub(crate) fn domain_txid(txid: &TransactionHash) -> domain::TransactionId {
    domain::TransactionId::from(txid.0)
}

pub(crate) fn local_txid(txid: domain::TransactionId) -> TransactionHash {
    TransactionHash(txid.into())
}

pub(crate) fn domain_scope(scope: ChainScope) -> zaino_chain::ChainScope {
    match scope {
        ChainScope::Finalised => zaino_chain::ChainScope::Finalised,
        ChainScope::FullChain => zaino_chain::ChainScope::FullChain,
    }
}

pub(crate) fn block_index(block: domain::BlockRef) -> BlockIndex {
    BlockIndex {
        height: local_height(block.height),
        hash: local_hash(block.hash),
    }
}

/// The transaction that spent an outpoint, or `None` if none in scope did.
///
/// A spend whose spender is unknown is an error: `None` would claim the output
/// unspent.
pub(crate) fn spender(status: SpendStatus) -> Result<Option<TransactionHash>, ChainIndexError> {
    match status {
        SpendStatus::Unspent => Ok(None),
        SpendStatus::SpentBy { txid, .. } => Ok(Some(local_txid(txid))),
        other => Err(ChainIndexError::internal(format!(
            "outpoint is spent but its spender cannot be named: {other:?}"
        ))),
    }
}

/// Where a transaction is, in the legacy location shapes.
pub(crate) fn chain_locations(
    status: zaino_service::TransactionStatus,
) -> Result<(Option<BestChainLocation>, HashSet<NonBestChainLocation>), ChainIndexError> {
    let at = |position: ChainTxPosition| {
        (
            local_hash(position.block.hash),
            local_height(position.block.height),
        )
    };
    let best = match status.best {
        None => None,
        Some(Placement::Block(position)) => {
            let (hash, height) = at(position);
            Some(BestChainLocation::Block(hash, height))
        }
        Some(Placement::Mempool(Some(height))) => {
            Some(BestChainLocation::Mempool(local_height(height)))
        }
        Some(Placement::Mempool(None)) => {
            return Err(ChainIndexError::internal(
                "a coherent mempool placement without a height",
            ))
        }
    };
    let elsewhere = status
        .elsewhere
        .into_iter()
        .map(|placement| match placement {
            Placement::Block(position) => {
                let (hash, height) = at(position);
                NonBestChainLocation::Block(hash, height)
            }
            Placement::Mempool(height) => NonBestChainLocation::Mempool(height.map(local_height)),
        })
        .collect();
    Ok((best, elsewhere))
}

/// A ChainView block, in the legacy shape.
///
/// The store's conversion takes a chainwork, so a placeholder stands in for it
/// and is replaced by the view's own: the real value, or `None` where the view
/// cannot know it. The legacy shape carries per-pool value balances the new
/// indexes no longer hold (D-97), so they are `None`.
pub(crate) fn indexed_block(block: ChainBlock) -> Result<IndexedBlock, ChainIndexError> {
    use zaino_chain_store_zainodb::adapter::{StoredBlock, StoredTx};
    let chainwork = block.chainwork;
    let stored = StoredBlock {
        header: block.header,
        transactions: block
            .transactions
            .into_iter()
            .map(StoredTx::transparent_only)
            .collect(),
        tree_roots: block.tree_roots,
        chainwork: chainwork.unwrap_or(domain::AbsoluteChainWork::new(NonZeroU128::MIN)),
    };
    Ok(
        zaino_chain_store_zainodb::adapter::indexed_block_from_stored(&stored)?
            .map_chainwork(|_| chainwork),
    )
}

/// The consensus branch a transaction at `height` is validated under.
pub(crate) fn branch_id(
    network: &zebra_chain::parameters::Network,
    height: domain::Height,
) -> Option<u32> {
    zebra_chain::parameters::ConsensusBranchId::current(
        network,
        zebra_chain::block::Height(u32::from(height)),
    )
    .map(u32::from)
}

/// The addresses of an address-index request, checked as zebrad 7 checks them
/// whichever backend answers: at most 10,000, and no TEX or malformed address.
///
/// A rejection keeps zebrad's code and message (`-5` for an address, `-8` for
/// the count) and is served as an invalid argument.
pub(crate) fn checked_addresses(
    request: &impl zebra_rpc::methods::ValidateAddresses,
) -> Result<Vec<domain::TransparentAddress>, ChainIndexError> {
    request
        .valid_addresses()
        .map_err(|error| {
            ChainIndexError::invalid_request(crate::error::LegacyRpcError {
                code: i64::from(error.code()),
                message: error.message().to_string(),
            })
        })?
        .into_iter()
        .map(|address| {
            domain::TransparentAddress::try_new(address.to_string())
                .map_err(ChainIndexError::validator_rejected)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn domain_ref(height: u32, tag: u8) -> domain::BlockRef {
        domain::BlockRef {
            hash: domain::BlockHash::from([tag; 32]),
            height: domain::Height::try_from(height).expect("test height is in range"),
        }
    }

    #[test]
    fn identifiers_survive_the_round_trip() {
        let hash = BlockHash([7; 32]);
        assert_eq!(local_hash(domain_hash(hash)), hash);

        let txid = TransactionHash([9; 32]);
        assert_eq!(local_txid(domain_txid(&txid)), txid);

        let height = Height(1_234);
        assert_eq!(
            domain_height(height).map(local_height),
            Some(height),
            "a valid height survives"
        );
    }

    #[test]
    fn a_height_beyond_the_protocol_maximum_names_no_block() {
        assert_eq!(domain_height(Height(u32::MAX)), None);
    }

    /// The branch a transaction is validated under flips from NU6.3 to NU7 at
    /// exactly the public Testnet activation height (ZIP 259), which is what
    /// `getblockchaininfo.consensus` and `GetLightdInfo` report at the boundary.
    #[test]
    fn branch_id_flips_to_nu7_at_the_testnet_activation_height() {
        const NU6_3: u32 = 0x37a5_165b;
        const NU7: u32 = 0x7719_0ad9;
        let testnet = zebra_chain::parameters::Network::new_default_testnet();
        let at = |height: u32| {
            branch_id(
                &testnet,
                domain::Height::try_from(height).expect("test height is in range"),
            )
        };

        assert_eq!(at(4_465_025), Some(NU6_3));
        assert_eq!(at(4_465_026), Some(NU7));
        assert_eq!(at(4_465_027), Some(NU7));
    }

    #[test]
    fn a_block_ref_becomes_a_block_index() {
        let index = block_index(domain_ref(5, 3));
        assert_eq!(index.height, Height(5));
        assert_eq!(index.hash, BlockHash([3; 32]));
    }

    #[test]
    fn a_spend_with_no_nameable_spender_is_an_error_not_unspent() {
        assert_eq!(spender(SpendStatus::Unspent).expect("unspent"), None);
        assert_eq!(
            spender(SpendStatus::SpentBy {
                txid: domain::TransactionId::from([4; 32]),
                input_index: 0,
                height: domain::Height::GENESIS,
            })
            .expect("spent"),
            Some(TransactionHash([4; 32]))
        );
        assert!(spender(SpendStatus::SpentSpenderUnknown).is_err());
    }

    #[test]
    fn locations_keep_best_and_competing_placements_apart() {
        let status = zaino_service::TransactionStatus {
            best: Some(Placement::Block(ChainTxPosition {
                block: domain_ref(10, 1),
                tx_index: 0,
            })),
            elsewhere: vec![
                Placement::Block(ChainTxPosition {
                    block: domain_ref(10, 2),
                    tx_index: 1,
                }),
                Placement::Mempool(None),
            ],
        };

        let (best, others) = chain_locations(status).expect("convertible");
        assert_eq!(
            best,
            Some(BestChainLocation::Block(BlockHash([1; 32]), Height(10)))
        );
        assert_eq!(
            others,
            HashSet::from([
                NonBestChainLocation::Block(BlockHash([2; 32]), Height(10)),
                NonBestChainLocation::Mempool(None),
            ])
        );
    }

    #[test]
    fn a_coherent_mempool_placement_keeps_its_height() {
        let height = zaino_primitives::types::Height::try_from(11).expect("small");
        let status = zaino_service::TransactionStatus {
            best: Some(Placement::Mempool(Some(height))),
            elsewhere: Vec::new(),
        };
        let (best, _) = chain_locations(status).expect("convertible");
        assert_eq!(best, Some(BestChainLocation::Mempool(Height(11))));
        let heightless = zaino_service::TransactionStatus {
            best: Some(Placement::Mempool(None)),
            elsewhere: Vec::new(),
        };
        assert!(chain_locations(heightless).is_err());
    }

    /// The legacy code a rejection carries, as the JSON-RPC layer recovers it.
    fn legacy_code(error: &ChainIndexError) -> Option<i64> {
        std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |error| {
            error.source()
        })
        .find_map(|error| error.downcast_ref::<crate::error::LegacyRpcError>())
        .map(|rejection| rejection.code)
    }

    fn checked(addresses: Vec<String>) -> ChainIndexError {
        checked_addresses(&zebra_rpc::client::GetAddressBalanceRequest::new(addresses))
            .expect_err("the request must be refused")
    }

    /// A malformed address and a TEX address are both zebrad's `-5`, served as
    /// an invalid argument.
    #[test]
    fn malformed_and_tex_addresses_are_refused_with_minus_five() {
        for address in [
            "not an address",
            "textest1qyqszqgpqyqszqgpqyqszqgpqyqszqgpfcjgfy",
        ] {
            let error = checked(vec![address.to_string()]);
            assert_eq!(legacy_code(&error), Some(-5), "{address}");
            assert_eq!(
                error.kind(),
                crate::error::ChainIndexErrorKind::InvalidArgument
            );
        }
    }

    /// More than 10,000 addresses is zebrad's `-8`, before any is parsed.
    #[test]
    fn too_many_addresses_are_refused_with_minus_eight() {
        let error = checked(vec!["not an address".to_string(); 10_001]);
        assert_eq!(legacy_code(&error), Some(-8));
        assert!(error.to_string().contains("too many addresses"), "{error}");
        assert_eq!(
            error.kind(),
            crate::error::ChainIndexErrorKind::InvalidArgument
        );
    }
}
