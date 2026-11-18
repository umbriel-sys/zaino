//! Transparent address reads answered locally (D-51): the store's effects to
//! its tip and the chain head's above it, shaped as the validator shapes them
//! and equal to its answers, without asking it.

mod common;

use common::{chain, height, rig, TIP};
use zaino_chain::{AddressRead as _, ChainView as _, ChainViewConfig, ChainViewError};
use zaino_primitives::types::{Network, TransparentAddress};
use zaino_source_mock::{address_of, MockChain, MINER_SCRIPT, RECIPIENT_SCRIPT};

/// The store's tip: below it the store answers, above it the head.
const STORE_TOP: u32 = TIP - 50;

fn address(script: &[u8], network: Network) -> TransparentAddress {
    TransparentAddress::try_new(address_of(script, network).expect("a standard script"))
        .expect("a valid address")
}

const ADDRESS_PORTS: [&str; 4] = [
    "get_address_balance",
    "get_address_utxos",
    "get_address_txids",
    "get_address_deltas",
];

fn rejected<T>(result: &Result<T, ChainViewError>) -> bool {
    matches!(result, Err(ChainViewError::Rejected(_)))
}

fn asked(source: &MockChain) -> bool {
    source
        .call_log()
        .iter()
        .any(|call| ADDRESS_PORTS.contains(&call.port))
}

#[tokio::test]
async fn address_reads_match_the_validator() {
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    // the head without extensions: every address read is the validator's
    let reference = rig(chain()).store_to(STORE_TOP).build().await;
    let source = chain();
    let snapshot = local
        .view_over(source.clone(), ChainViewConfig::default())
        .snapshot();
    let expected = reference.view().snapshot();

    let miner = address(&MINER_SCRIPT, Network::Regtest);
    let recipient = address(&RECIPIENT_SCRIPT, Network::Regtest);
    let queries = [
        vec![miner.clone()],
        vec![recipient.clone()],
        vec![miner.clone(), recipient.clone(), miner.clone()],
    ];
    let ranges = [
        (0, TIP),
        (1, STORE_TOP),
        (STORE_TOP - 5, STORE_TOP + 5),
        (STORE_TOP + 1, TIP),
        (TIP, TIP),
    ];
    for addresses in &queries {
        let balance = snapshot.address_balance(addresses).await.expect("answered");
        assert!(
            u64::from(balance.balance) > 0,
            "{addresses:?} hold something"
        );
        assert_eq!(
            balance,
            expected.address_balance(addresses).await.expect("answered"),
            "balance of {addresses:?}"
        );
        assert_eq!(
            snapshot.address_utxos(addresses).await.expect("answered"),
            expected.address_utxos(addresses).await.expect("answered"),
            "UTXOs of {addresses:?}"
        );
        for (start, end) in ranges {
            let (start, end) = (height(start), height(end));
            let deltas = snapshot
                .address_deltas(addresses, start, end)
                .await
                .expect("answered");
            assert_eq!(
                deltas,
                expected
                    .address_deltas(addresses, start, end)
                    .await
                    .expect("answered"),
                "deltas of {addresses:?} over {start}..={end}"
            );
            assert_eq!(
                snapshot
                    .address_txids(addresses, start, end)
                    .await
                    .expect("answered"),
                expected
                    .address_txids(addresses, start, end)
                    .await
                    .expect("answered"),
                "txids of {addresses:?} over {start}..={end}"
            );
        }
    }
    assert!(!asked(&source), "answered without the validator");
}

/// The validator's refusals are the local path's: an inverted range, txids
/// past the tip, an address for another network.
#[tokio::test]
async fn address_reads_refuse_as_the_validator_does() {
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    let reference = rig(chain()).store_to(STORE_TOP).build().await;
    let source = chain();
    let snapshot = local
        .view_over(source.clone(), ChainViewConfig::default())
        .snapshot();
    let expected = reference.view().snapshot();
    let miner = vec![address(&MINER_SCRIPT, Network::Regtest)];
    let foreign = vec![address(&MINER_SCRIPT, Network::Mainnet)];
    let (inverted, past) = ((height(10), height(5)), (height(1), height(TIP + 1)));
    for (start, end) in [inverted, past] {
        assert_eq!(
            rejected(&snapshot.address_txids(&miner, start, end).await),
            rejected(&expected.address_txids(&miner, start, end).await),
            "txids over {start}..={end}"
        );
        assert_eq!(
            rejected(&snapshot.address_deltas(&miner, start, end).await),
            rejected(&expected.address_deltas(&miner, start, end).await),
            "deltas over {start}..={end}"
        );
    }
    assert!(rejected(&snapshot.address_balance(&foreign).await));
    assert!(rejected(&expected.address_balance(&foreign).await));
    assert!(rejected(&snapshot.address_utxos(&foreign).await));
    assert!(rejected(
        &snapshot
            .address_deltas(&foreign, height(0), height(TIP))
            .await
    ));
    assert!(!asked(&source), "refused without the validator");
}

/// The manifest advertises local address history only where both halves
/// build it.
#[tokio::test]
async fn the_manifest_reports_address_history_from_both_halves() {
    use zaino_chain::{Answerable, ChainCapability};
    let mut config = ChainViewConfig::default();
    config.passthrough_enabled = false;
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    assert_eq!(
        local
            .view_with(config.clone())
            .serviceability()
            .get(ChainCapability::AddressHistory),
        Answerable::ToHeight(height(TIP))
    );
    let plain = rig(chain()).store_to(STORE_TOP).build().await;
    assert_eq!(
        plain
            .view_with(config)
            .serviceability()
            .get(ChainCapability::AddressHistory),
        Answerable::Absent
    );
}
