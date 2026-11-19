//! Transparent address reads answered locally (D-51): the store's effects to
//! its tip and the chain head's above it, shaped as the validator shapes them
//! and equal to its answers, without asking it.

mod common;

use common::{chain, height, rig, TIP};
use zaino_chain::{AddressRead as _, ChainView as _, ChainViewConfig, ChainViewError};
use zaino_primitives::types::{Height, Network, TransparentAddress};
use zaino_source::UtxoPage;
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

fn invalid<T>(result: &Result<T, ChainViewError>, start: Height, end: Height) -> bool {
    matches!(result, Err(ChainViewError::InvalidRange { start: s, end: e }) if *s == start && *e == end)
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
        for page in [
            UtxoPage::default(),
            UtxoPage::new(height(150), 2).expect("in range"),
            UtxoPage::new(height(290), 0).expect("in range"),
        ] {
            let local = snapshot
                .address_utxos(addresses, page)
                .await
                .expect("answered");
            assert_eq!(
                local,
                expected
                    .address_utxos(addresses, page)
                    .await
                    .expect("answered"),
                "UTXOs of {addresses:?} {page:?}"
            );
            assert!(local.iter().all(|utxo| utxo.height >= page.start()));
            assert!(page
                .max_entries()
                .is_none_or(|max| local.len() <= max.get()));
        }
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
        assert!(
            invalid(
                &snapshot.address_txids(&miner, start, end).await,
                start,
                end
            ),
            "local txids over {start}..={end}"
        );
        assert!(
            invalid(
                &expected.address_txids(&miner, start, end).await,
                start,
                end
            ),
            "validator txids over {start}..={end}"
        );
    }
    let (start, end) = inverted;
    assert!(
        invalid(
            &snapshot.address_deltas(&miner, start, end).await,
            start,
            end
        ),
        "local deltas over {start}..={end}"
    );
    assert!(
        invalid(
            &expected.address_deltas(&miner, start, end).await,
            start,
            end
        ),
        "validator deltas over {start}..={end}"
    );
    let (start, end) = past;
    assert_eq!(
        snapshot.address_deltas(&miner, start, end).await.is_ok(),
        expected.address_deltas(&miner, start, end).await.is_ok(),
        "deltas over {start}..={end}"
    );
    assert!(rejected(&snapshot.address_balance(&foreign).await));
    assert!(rejected(&expected.address_balance(&foreign).await));
    assert!(rejected(
        &snapshot.address_utxos(&foreign, UtxoPage::default()).await
    ));
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

/// Rows of address history (receives and spends) for `script` at heights
/// `1..=through`, read off the chain: genesis outputs are in no history.
fn history_rows(chain: &MockChain, script: &[u8], through: u32) -> usize {
    let blocks: Vec<_> = (1..=through)
        .map(|h| common::block_at(chain, h).block)
        .collect();
    let created: std::collections::HashMap<_, _> = blocks
        .iter()
        .flat_map(|block| block.transactions().to_vec())
        .flat_map(|tx| {
            (0u32..)
                .zip(tx.transparent.outputs)
                .map(move |(index, output)| ((tx.txid, index), output.script))
        })
        .collect();
    let receives = created
        .values()
        .filter(|paid| paid.as_bytes() == script)
        .count();
    let spends = blocks
        .iter()
        .flat_map(|block| block.transactions().to_vec())
        .flat_map(|tx| tx.transparent.inputs)
        .filter(|input| {
            created
                .get(&(input.prev_txid, input.prev_index))
                .is_some_and(|paid| paid.as_bytes() == script)
        })
        .count();
    receives + spends
}

/// One row budget covers the store and the window together: the store's rows
/// leave the window only the rest, and a query past the budget is refused,
/// not truncated and not sent to the validator (K-01).
#[tokio::test]
async fn a_query_past_the_row_budget_is_refused() {
    let local = rig(chain())
        .store_to(STORE_TOP)
        .extended_head()
        .build()
        .await;
    let rows = history_rows(&local.chain, &MINER_SCRIPT, TIP);
    let stored = history_rows(&local.chain, &MINER_SCRIPT, STORE_TOP);
    // the store alone fits; only the window's share overruns
    assert!(stored < rows - 1, "{stored} of {rows}");
    let miner = vec![address(&MINER_SCRIPT, Network::Regtest)];

    let source = chain();
    let within = local
        .view_over(
            source.clone(),
            ChainViewConfig::default().with_max_address_scan_rows(rows),
        )
        .snapshot()
        .address_utxos(&miner, UtxoPage::default())
        .await;
    assert!(within.is_ok(), "{within:?}");

    let past = local
        .view_over(
            source.clone(),
            ChainViewConfig::default().with_max_address_scan_rows(rows - 1),
        )
        .snapshot()
        .address_utxos(&miner, UtxoPage::default())
        .await;
    assert!(rejected(&past), "{past:?}");
    assert!(!asked(&source), "the validator is never asked");
}
