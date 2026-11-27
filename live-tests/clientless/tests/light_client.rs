//! Zaino's light-client service against zebrad: `GetBlockHeaders` serves
//! zebrad's header bytes under the block ranges' rules, and a
//! `GetHistoryProof` recomputes the commitment in the tip's real header.

use std::time::Duration;

use anyhow::{Context as _, Result};
use rstest::rstest;
use serde_json::json;
use zaino_indexes::history_tree::{root_of, Epoch};
use zaino_proto::proto::light_client::{HistoryNode, HistoryProofRequest};
use zaino_proto::proto::service::{BlockId, BlockRange};
use zaino_testutils::{Backend, ChainCache, Validator as _, Zainod, Zebrad};
use zcash_history::{Entry, Version as _, V3};

const READY: Duration = Duration::from_secs(180);

fn at(height: u32) -> Option<BlockId> {
    Some(BlockId {
        height: u64::from(height),
        hash: Vec::new(),
    })
}

fn personalized(personal: &[u8; 16], parts: &[&[u8]]) -> [u8; 32] {
    let mut state = blake2b_simd::Params::new()
        .hash_length(32)
        .personal(personal)
        .to_state();
    for part in parts {
        state.update(part);
    }
    let mut hash = [0u8; 32];
    hash.copy_from_slice(state.finalize().as_bytes());
    hash
}

#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn block_headers_are_zebrads(#[case] backend: Backend) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    let vrpc = zebrad.rpc();
    let mut client = indexer.light_client();
    for (start, end) in [(1, 4), (tip - 3, tip), (tip, tip - 3), (tip - 1, tip + 2)] {
        let mut stream = client
            .get_block_headers(BlockRange {
                start: at(start),
                end: at(end),
                pool_types: Vec::new(),
            })
            .await?
            .into_inner();
        let mut heights = Vec::new();
        let refusal = loop {
            match stream.message().await {
                Ok(Some(header)) => {
                    let height = u32::try_from(header.height)?;
                    let hash = vrpc
                        .call_value("getblockhash", json!([height]))
                        .await?
                        .as_str()
                        .context("a hash")?
                        .to_owned();
                    let mut served_hash = header.hash.clone();
                    served_hash.reverse();
                    assert_eq!(hex::encode(served_hash), hash, "{height}");
                    let raw = vrpc
                        .call_value("getblockheader", json!([hash, false]))
                        .await?;
                    assert_eq!(json!(hex::encode(&header.header)), raw, "{height}");
                    heights.push(height);
                }
                Ok(None) => break None,
                Err(refusal) => break Some(refusal),
            }
        };
        let last = end.min(tip);
        let expected: Vec<u32> = match start <= end {
            true => (start..=last).collect(),
            false => (end..=start).rev().collect(),
        };
        assert_eq!(heights, expected, "{start}..{end}");
        assert_eq!(
            refusal.map(|refusal| refusal.code()),
            (end > tip).then_some(tonic::Code::OutOfRange),
            "{start}..{end}"
        );
    }
    Ok(())
}

#[rstest]
#[case::fetch(Backend::Fetch)]
#[case::state(Backend::State)]
#[tokio::test(flavor = "multi_thread")]
async fn a_history_proof_recomputes_the_tip_headers_commitment(
    #[case] backend: Backend,
) -> Result<()> {
    let cache = ChainCache::ensure().await?;
    let zebrad = Zebrad::launch(cache.zebrad_config()).await?;
    let indexer = Zainod::launch(cache.indexer_config(&zebrad, backend)).await?;
    let tip = cache.manifest.fixture.height;
    indexer.wait_for_block(tip, READY).await?;

    let vrpc = zebrad.rpc();
    let mut client = indexer.light_client();
    for (block, against) in [(2, tip), (tip / 2, tip), (tip - 1, tip), (10, 1_000)] {
        let proof = client
            .get_history_proof(HistoryProofRequest {
                block: at(block),
                tip: at(against),
            })
            .await?
            .into_inner();
        assert_eq!(proof.tip_height, u64::from(against));
        let branch = proof.consensus_branch_id;
        let data = |node: &HistoryNode| -> Result<_> {
            Ok(Entry::<V3>::from_bytes(branch, &node.node)?.data().clone())
        };

        let leaf = proof.leaf.as_ref().context("a leaf")?;
        let (mut position, mut node) = (leaf.position, data(leaf)?);
        for sibling in &proof.path {
            (position, node) = match sibling.position < position {
                true => (position + 1, V3::combine(&data(sibling)?, &node)),
                false => (sibling.position + 1, V3::combine(&node, &data(sibling)?)),
            };
        }
        let peak = proof
            .peaks
            .iter()
            .find(|peak| peak.position == position)
            .with_context(|| format!("{block}: the path ends at {position}, not a peak"))?;
        assert_eq!(V3::hash(&node), V3::hash(&data(peak)?), "{block}");

        let history_root = root_of(&Epoch {
            branch,
            start: 0,
            length: proof.tree_length,
            peaks: proof
                .peaks
                .iter()
                .map(|peak| (peak.position, peak.node.clone()))
                .collect(),
        })?;

        let mut tip_hash = proof.tip_hash.clone();
        tip_hash.reverse();
        let tip_hash = hex::encode(tip_hash);
        let verbose = vrpc.call_value("getblock", json!([tip_hash, 2])).await?;
        let mut digests = Vec::new();
        for tx in verbose["tx"].as_array().context("transactions")? {
            let mut digest = hex::decode(tx["authdigest"].as_str().context("an auth digest")?)?;
            digest.reverse();
            digests.push(<[u8; 32]>::try_from(digest.as_slice())?);
        }
        digests.resize(digests.len().next_power_of_two(), [0u8; 32]);
        while digests.len() > 1 {
            digests = digests
                .chunks(2)
                .map(|pair| personalized(b"ZcashAuthDatHash", &[&pair[0], &pair[1]]))
                .collect();
        }
        let commitment = personalized(
            b"ZcashBlockCommit",
            &[&history_root, &digests[0], &[0u8; 32]],
        );

        let header = vrpc
            .call_value("getblockheader", json!([tip_hash, false]))
            .await?;
        let header = hex::decode(header.as_str().context("a raw header")?)?;
        assert_eq!(
            hex::encode(&header[68..100]),
            hex::encode(commitment),
            "{block} against {against}"
        );
    }

    let refused = client
        .get_history_proof(HistoryProofRequest {
            block: at(tip),
            tip: at(tip),
        })
        .await
        .expect_err("the tip is not below itself");
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    Ok(())
}
