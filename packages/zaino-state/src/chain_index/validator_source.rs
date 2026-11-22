//! ChainIndex's validator: the `zaino-source` validator it is built on, how
//! it is connected, and the legacy error shapes its refusals take.

use std::sync::Arc;

use zaino_source::QueryError;
use zaino_source_zebra::ZebraValidator;

use super::source::BlockchainSourceError;
use super::source_ports::ChainIndexSourcePorts;

/// ChainIndex's validator source, backed by the `zaino-source` stack.
///
/// The single adapter between ChainIndex and the driving ports
/// ([`zaino_source`], in domain types). It is generic over the source so that
/// conversion is written once: the production composite and the test mocks
/// reach ChainIndex through the same code, which makes the mock-backed suites
/// coverage of the conversion layer rather than of a parallel implementation
/// of it.
///
/// [`ZebraValidatorSource`] is the production instantiation.
pub struct ValidatorSource<V> {
    /// Shared because the port requires `Clone` and a source may own
    /// connections and a database handle that must not be duplicated.
    validator: Arc<V>,

    /// Needed to render verbose transactions: their presentation depends on the
    /// network's consensus branch schedule, which zebra's builder takes as an
    /// argument.
    network: zebra_chain::parameters::Network,

    /// Zebra's own tip-change stream, when this deployment reads the state
    /// database directly.
    ///
    /// Held rather than synthesised: the port hands out
    /// `zebra_state::ChainTipChange`, which cannot be built from the domain
    /// tip subscription. Keeping the real one preserves today's behaviour
    /// exactly, including that RPC-only deployments have none.
    chain_tip_change: Option<zebra_state::ChainTipChange>,
}

/// ChainIndex's source as deployed against a Zebra validator.
pub type ZebraValidatorSource = ValidatorSource<ZebraValidator>;

impl<V: ChainIndexSourcePorts> ValidatorSource<V> {
    /// Wrap anything answering ChainIndex's questions as its source.
    pub fn new(
        validator: V,
        network: zebra_chain::parameters::Network,
        chain_tip_change: Option<zebra_state::ChainTipChange>,
    ) -> Self {
        Self {
            validator: Arc::new(validator),
            network,
            chain_tip_change,
        }
    }

    /// The backing source.
    ///
    /// Tests drive a mock through a control surface that is deliberately not
    /// on any port — mining blocks is something a test fixture does, not a
    /// question a validator answers — so they need the concrete type back.
    pub fn source(&self) -> &V {
        &self.validator
    }

    /// The underlying validator, shared.
    ///
    /// A subsystem driven by its own runtime — ChainHead, and the mempool after
    /// it — holds the validator directly rather than through this wrapper,
    /// because it speaks the `zaino-source` ports and does not need the
    /// wire-typed scaffolding this type provides. Sharing rather than cloning
    /// matters: a validator may own connections and a database handle.
    pub(crate) fn validator(&self) -> Arc<V> {
        Arc::clone(&self.validator)
    }

    /// Zebra's tip-change stream, when this deployment reads its state.
    pub(crate) fn chain_tip_change(&self) -> Option<zebra_state::ChainTipChange> {
        self.chain_tip_change.clone()
    }
}

#[cfg(feature = "test_dependencies")]
impl ZebraValidatorSource {
    /// The backing state service, when this deployment reads the database.
    ///
    /// Test-only escape hatch, carried over from the deleted connector: live
    /// tests recompute expected chain data straight off the service.
    pub fn read_state_service(&self) -> Option<&zebra_state::ReadStateService> {
        self.validator
            .read_state()
            .map(|adapter| adapter.read_state_service())
    }
}

/// Written out rather than derived: `derive(Clone)` would demand `V: Clone`,
/// but the source is held behind an `Arc` precisely so it need not be — it owns
/// connections and a database handle that must not be duplicated.
impl<V> Clone for ValidatorSource<V> {
    fn clone(&self) -> Self {
        Self {
            validator: Arc::clone(&self.validator),
            network: self.network.clone(),
            chain_tip_change: self.chain_tip_change.clone(),
        }
    }
}

impl<V> std::fmt::Debug for ValidatorSource<V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatorSource")
            .field("network", &self.network)
            .field("has_tip_stream", &self.chain_tip_change.is_some())
            .finish_non_exhaustive()
    }
}

/// Flatten a port error into the scaffolding port's single error type.
///
/// The ports distinguish a domain rejection from a transport fault; this error
/// cannot represent that, so the kind is kept in the message rather than lost.
/// The distinction becomes usable again as consumers move onto the port errors.
///
/// The transport fault is carried as the error's `source` rather than only
/// formatted into the message. A serving layer recovers legacy-compatible RPC
/// error codes by downcast-walking [`std::error::Error::source`], so
/// flattening a [`NonDomainError`] to
/// a string would strip the [`FailureMode::RpcError`] code those clients key
/// on.
///
/// A domain rejection is carried the same way, as a typed
/// [`LegacyRpcError`](crate::error::LegacyRpcError) with the legacy full node's
/// `InvalidParameter`. That is the code the legacy full node itself answers "not found" with,
/// and it is what reached clients before the adapters learned to tell a missing
/// object from an unreachable node — the reclassification must not cost the
/// served interface its error code.
///
/// [`NonDomainError`]: zaino_source::NonDomainError
/// [`FailureMode::RpcError`]: zaino_source::FailureMode::RpcError
pub(crate) fn err<E, N>(error: QueryError<E, N>) -> BlockchainSourceError
where
    E: std::fmt::Debug + std::fmt::Display,
    N: std::error::Error + Send + Sync + 'static,
{
    match error {
        QueryError::Domain(e) => BlockchainSourceError::unrecoverable_context(
            "validator rejected the query",
            crate::error::LegacyRpcError::new(
                zebra_rpc::server::error::LegacyCode::InvalidParameter,
                e.to_string(),
            ),
        ),
        QueryError::NonDomain(e) => {
            BlockchainSourceError::unrecoverable_context("validator unreachable", e)
        }
    }
}

/// [`err`], for a submission: a rejection keeps the validator's own code and
/// message.
fn send_err<N>(error: QueryError<zaino_source::SendRawTransactionError, N>) -> BlockchainSourceError
where
    N: std::error::Error + Send + Sync + 'static,
{
    match error {
        QueryError::Domain(rejection) => BlockchainSourceError::unrecoverable_context(
            "validator rejected the transaction",
            crate::error::LegacyRpcError {
                code: rejection.legacy_code(),
                message: rejection.message().to_owned(),
            },
        ),
        fetch @ QueryError::NonDomain(_) => err(fetch),
    }
}

/// Flatten a `getspentinfo` port error, preserving the legacy full node's own error code.
///
/// The generic [`err`] reports every domain rejection as `InvalidParameter`
/// (`-8`), which is right where the interface has no more specific code to
/// offer. `getspentinfo` does: the legacy full node answers "no spend on record" with `-5`
/// (`InvalidAddressOrKey`) and the message `Unable to get spent info`, and that
/// is the pair a client matches on.
///
/// Neither Zaino nor its predecessor served `-5` here. The old path consumed the
/// code into a typed error, destroying the `RpcError` the serving layer walks
/// for, so a not-spent answer arrived as a generic internal error; the first
/// rewrite of this method then reported `-8`. Both are wrong in the same
/// direction — the code the client is looking for never reached it.
///
/// `Unsupported` has no the legacy full node code, because the legacy full node always implements the
/// method. It is reported as `-32601`, the envelope's own "method not found",
/// which is what the client would have seen had it asked the validator
/// directly — and is deliberately *not* `-5`: "this node cannot answer" must
/// not be read as "the output is unspent".
pub(crate) fn spent_info_err<N: std::error::Error + Send + Sync + 'static>(
    error: QueryError<zaino_source::GetSpentInfoError, N>,
) -> BlockchainSourceError {
    use zaino_source::GetSpentInfoError;

    let (code, message) = match error {
        QueryError::Domain(rejection @ GetSpentInfoError::NotSpent) => (
            zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey as i64,
            rejection.to_string(),
        ),
        QueryError::Domain(rejection @ GetSpentInfoError::Unsupported) => {
            return method_missing(rejection)
        }
        // A transport fault is not an answer; `err` already carries it with the
        // typed cause the serving layer needs.
        fetch @ QueryError::NonDomain(_) => return err(fetch),
    };

    BlockchainSourceError::unrecoverable_context(
        "validator rejected the query",
        crate::error::LegacyRpcError { code, message },
    )
}

/// Flatten a `getstandardfee` port error, keeping `Unsupported` as `-32601`.
fn standard_fee_err<N: std::error::Error + Send + Sync + 'static>(
    error: QueryError<zaino_source::GetStandardFeeError, N>,
) -> BlockchainSourceError {
    match error {
        QueryError::Domain(rejection @ zaino_source::GetStandardFeeError::Unsupported) => {
            method_missing(rejection)
        }
        other => err(other),
    }
}

/// A validator's refusal, kept typed by the service: the legacy shape its port
/// had, its specific code where the port has one.
pub(crate) fn rejected_err(
    error: Box<dyn std::error::Error + Send + Sync + 'static>,
) -> BlockchainSourceError {
    type Domain<E> = QueryError<E, zaino_source::NonDomainError>;
    if let Some(rejection) = error.downcast_ref::<zaino_source::SendRawTransactionError>() {
        return send_err(Domain::Domain(rejection.clone()));
    }
    if let Some(rejection) = error.downcast_ref::<zaino_source::GetStandardFeeError>() {
        return standard_fee_err(Domain::Domain(rejection.clone()));
    }
    err(Domain::Domain(error))
}

/// A validator that does not implement the method, reported as the envelope's
/// `-32601` — what the client would have seen asking the validator directly.
pub(crate) fn method_missing(rejection: impl std::fmt::Display) -> BlockchainSourceError {
    BlockchainSourceError::unrecoverable_context(
        "validator rejected the query",
        crate::error::LegacyRpcError {
            code: crate::error::LegacyRpcError::METHOD_NOT_FOUND,
            message: rejection.to_string(),
        },
    )
}

/// A domain height from a bare `u32` request field.
pub(crate) fn domain_height(
    h: u32,
) -> Result<zaino_primitives::types::Height, BlockchainSourceError> {
    zaino_primitives::types::Height::try_from(h)
        .map_err(|e| BlockchainSourceError::Unrecoverable(e.to_string()))
}

fn invalid(message: String) -> BlockchainSourceError {
    BlockchainSourceError::Unrecoverable(message)
}

/// Parse a txid written in RPC display order.
pub(crate) fn parse_display_txid(
    hex_str: &str,
) -> Result<zaino_primitives::types::TransactionId, BlockchainSourceError> {
    let bytes = hex::decode(hex_str).map_err(|e| invalid(format!("txid is not hex: {e}")))?;
    let mut internal: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("txid is not 32 bytes".to_string()))?;
    internal.reverse();
    Ok(zaino_primitives::types::TransactionId::from(internal))
}

/// Parse a 32-byte identifier written in RPC display order.
pub(crate) fn parse_display_hash32(hex_str: &str) -> Result<[u8; 32], BlockchainSourceError> {
    let bytes = hex::decode(hex_str).map_err(|e| invalid(format!("not hex: {e}")))?;
    let mut internal: [u8; 32] = bytes
        .try_into()
        .map_err(|_| invalid("identifier is not 32 bytes".to_string()))?;
    internal.reverse();
    Ok(internal)
}

// ---------------------------------------------------------------------------
// Mempool ports, forwarded
//
// The mempool subsystem reads the validator through `zaino-source` directly
// rather than through `BlockchainSource`, so these four pass straight through
// with no conversion — there is nothing to convert, both sides already speak
// domain types. They exist because ChainIndex hands its source to the mempool,
// and a trait impl does not travel through a wrapper on its own.
//
// All four must reach the *same* transport, which they do: `ZebraValidator`
// routes them together, and the mempool's coherence check depends on it.
// ---------------------------------------------------------------------------

impl<V: ChainIndexSourcePorts> zaino_source::ValidatorSource for ValidatorSource<V> {
    // Pass the inner validator's non-domain type straight through; this legacy
    // wrapper only delegates.
    type NonDomain = <V as zaino_source::ValidatorSource>::NonDomain;
}

impl<V: ChainIndexSourcePorts> zaino_source::OneShotGetMempoolTxids for ValidatorSource<V> {
    async fn get_mempool_txids(
        &self,
    ) -> Result<
        Vec<zaino_primitives::types::TransactionId>,
        zaino_source::QueryError<zaino_source::GetMempoolTxidsError, Self::NonDomain>,
    > {
        self.validator.get_mempool_txids().await
    }
}

impl<V: ChainIndexSourcePorts> zaino_source::OneShotGetMempoolMetadata for ValidatorSource<V> {
    async fn get_mempool_metadata(
        &self,
    ) -> Result<
        Vec<zaino_source::MempoolTxMeta>,
        zaino_source::QueryError<zaino_source::GetMempoolMetadataError, Self::NonDomain>,
    > {
        self.validator.get_mempool_metadata().await
    }
}

impl<V: ChainIndexSourcePorts> zaino_source::OneShotGetRawMempoolTransaction
    for ValidatorSource<V>
{
    async fn get_raw_mempool_transaction(
        &self,
        txid: zaino_primitives::types::TransactionId,
    ) -> Result<
        Vec<u8>,
        zaino_source::QueryError<zaino_source::GetRawMempoolTransactionError, Self::NonDomain>,
    > {
        self.validator.get_raw_mempool_transaction(txid).await
    }
}

impl<V: ChainIndexSourcePorts> zaino_source::OneShotGetMempoolSourceTip for ValidatorSource<V> {
    async fn get_mempool_source_tip(
        &self,
    ) -> Result<
        (
            zaino_primitives::types::BlockHash,
            zaino_primitives::types::Height,
        ),
        zaino_source::QueryError<std::convert::Infallible, Self::NonDomain>,
    > {
        self.validator.get_mempool_source_tip().await
    }
}

impl ZebraValidatorSource {
    /// Build an RPC-only source from a validator endpoint.
    ///
    /// Skips the startup handshake that [`spawn_rpc`](Self::spawn_rpc) performs
    /// — no first-block wait, no network adoption — so the caller must already
    /// know the network and that the validator is serving. Intended for tests
    /// and for embedders that have done both themselves.
    pub fn rpc_only(
        rpc_address: &str,
        auth: Option<(String, String)>,
        network: zebra_chain::parameters::Network,
    ) -> Result<Self, BlockchainSourceError> {
        let client = zaino_rpc::RpcClient::new(zaino_rpc::RpcClientConfig {
            url: format!("http://{rpc_address}"),
            auth,
            ..Default::default()
        })
        .map_err(|e| invalid(format!("cannot build the validator RPC client: {e}")))?;

        Ok(Self::new(
            zaino_source_zebra::ZebraValidator::rpc_only(zaino_source_zebra::rpc_adapter(client)),
            network,
            None,
        ))
    }

    /// Connect to a validator over JSON-RPC alone.
    ///
    /// Blocks until the validator can serve a tip. A freshly started validator
    /// answers RPC before it has committed its first block — Zebra serves
    /// `getblockchaininfo` and an empty mempool while `getbestblockhash` still
    /// reports no blocks, which can take minutes of peer discovery. Everything
    /// downstream assumes a servable tip, so waiting here is what stops spawn
    /// failing and exit-looping the whole process.
    pub async fn spawn_rpc(
        common: &crate::config::CommonBackendConfig,
    ) -> Result<
        (
            Self,
            zaino_primitives::types::rpc::NodeInfo,
            zebra_chain::parameters::Network,
        ),
        BlockchainSourceError,
    > {
        let adapter = rpc_adapter(common)?;
        let info = zaino_source::OneShotGetNodeInfo::get_node_info(&adapter)
            .await
            .map_err(err)?;

        while let Err(e) = zaino_source::OneShotGetChainTip::get_chain_tip(&adapter).await {
            tracing::info!(%e, "Waiting for validator to serve its first block");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        }

        // Adopted before anything consumes a `Network`, so the index and its
        // validator cannot disagree about where an upgrade activates.
        //
        // This is the first place in Zaino that actually constructs a
        // `ValidatorClient` (the sealed resilient client): `adopt_network`
        // binds the canonical `GetBlockchainInfo` port, which only
        // `ValidatorClient<V>` implements, so the adapter has to be wrapped
        // here. Wrapping buys resilience to transient unreachability for this
        // one boot RPC — not readiness: waiting for the validator to *become*
        // ready is a lifecycle concern the caller owns (that is what the
        // tip-serving wait above does), never the client's. The other
        // `OneShotGet*` calls here are single attempts on the bare adapter.
        let source = zaino_source::ValidatorClient::new(
            rpc_adapter(common)?,
            zaino_source::RetryPolicy::default(),
        );
        let network = adopt_network(common, &source).await?;

        let validator = zaino_source_zebra::ZebraValidator::rpc_only(adapter);

        Ok((Self::new(validator, network.clone(), None), info, network))
    }

    /// Connect to a validator and additionally read its state database directly.
    ///
    /// Launches the chain syncer and waits for it to catch up before returning.
    /// The wait compares tip **hash** as well as height: the same height can
    /// name different blocks during a reorg, so height alone would report a
    /// false match and hand back a service that disagrees with the validator.
    pub async fn spawn_direct(
        common: &crate::config::CommonBackendConfig,
        direct: &crate::config::DirectConnectionConfig,
    ) -> Result<
        (
            Self,
            zaino_primitives::types::rpc::NodeInfo,
            zebra_chain::parameters::Network,
        ),
        BlockchainSourceError,
    > {
        use futures::TryFutureExt as _;
        use tower::{Service as _, ServiceExt as _};
        use zebra_state::{ReadRequest, ReadResponse};

        let adapter = rpc_adapter(common)?;
        let info = zaino_source::OneShotGetNodeInfo::get_node_info(&adapter)
            .await
            .map_err(err)?;

        // As in `spawn_rpc`: `adopt_network` needs the sealed resilient port,
        // so wrap the adapter in a `ValidatorClient`. The `OneShotGet*` call
        // above is a single attempt on the bare adapter (see the note there).
        let source = zaino_source::ValidatorClient::new(
            rpc_adapter(common)?,
            zaino_source::RetryPolicy::default(),
        );
        let network = adopt_network(common, &source).await?;

        tracing::info!(
            grpc_address = %direct.validator_grpc_address,
            "Launching Chain Syncer"
        );
        let (mut read_state_service, _latest_chain_tip, chain_tip_change, sync_task_handle) =
            zebra_rpc::sync::init_read_state_with_syncer(
                direct.validator_state_config.clone(),
                &network,
                direct.validator_grpc_address,
            )
            .await
            .map_err(|e| invalid(e.to_string()))?
            .map_err(|e| invalid(e.to_string()))?;

        tracing::info!("Chain syncer launched");

        loop {
            let (validator_hash, validator_height) =
                zaino_source::OneShotGetChainTip::get_chain_tip(&adapter)
                    .await
                    .map_err(err)?;

            let response = read_state_service
                .ready()
                .and_then(|service| service.call(ReadRequest::Tip))
                .await
                .map_err(BlockchainSourceError::unrecoverable)?;

            let ReadResponse::Tip(tip) = response else {
                return Err(invalid("unexpected response to a Tip request".to_string()));
            };

            // As above: the syncer has no tip until genesis arrives, so this is
            // a wait rather than a failure.
            let Some((syncer_height, syncer_tip_hash)) = tip else {
                tracing::info!("Waiting for validator to serve its first block");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                continue;
            };

            if u32::from(validator_height) == syncer_height.0
                && <[u8; 32]>::from(validator_hash) == syncer_tip_hash.0
            {
                tracing::info!(
                    height = syncer_height.0,
                    tip_hash = %syncer_tip_hash,
                    "ReadStateService synced with Zebra"
                );
                break;
            }

            tracing::info!(
                syncer_height = syncer_height.0,
                validator_height = u32::from(validator_height),
                syncer_tip_hash = %syncer_tip_hash,
                validator_tip_hash = ?validator_hash,
                "ReadStateService syncing with Zebra"
            );
            tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
        }

        let validator = zaino_source_zebra::ZebraValidator::with_read_state(
            adapter,
            zaino_source_zebra::ZebraReadStateAdapter::from_service(
                read_state_service,
                &network,
                Some(Arc::new(sync_task_handle)),
            ),
        );

        Ok((
            Self::new(validator, network.clone(), Some(chain_tip_change)),
            info,
            network,
        ))
    }
}

/// The runtime network, adopted from the validator's schedule.
async fn adopt_network(
    common: &crate::config::CommonBackendConfig,
    source: &impl zaino_source::GetBlockchainInfo,
) -> Result<zebra_chain::parameters::Network, BlockchainSourceError> {
    zaino_source_zebra::adopt_network(common.network, &common.validator_rpc_address, source)
        .await
        .map_err(|error| BlockchainSourceError::Unrecoverable(error.to_string()))
}

/// The JSON-RPC client for the new source stack.
///
/// Cookie auth is resolved to a credential pair here. The connector this
/// replaced read the cookie file once at construction and stored the token, and
/// `Basic` auth over `("__cookie__", token)` produces the identical header, so
/// this preserves that behaviour exactly rather than re-reading per request.
fn rpc_client(
    common: &crate::config::CommonBackendConfig,
) -> Result<zaino_rpc::RpcClient, BlockchainSourceError> {
    let auth = zaino_rpc::auth_from_parts(
        common.validator_cookie_path.as_deref(),
        common.validator_rpc_user.clone(),
        common.validator_rpc_password.clone(),
    )
    .map_err(|e| invalid(e.to_string()))?;

    zaino_rpc::RpcClient::new(zaino_rpc::RpcClientConfig {
        url: format!("http://{}", common.validator_rpc_address),
        auth,
        ..Default::default()
    })
    .map_err(|e| invalid(format!("cannot build the validator RPC client: {e}")))
}

/// The JSON-RPC adapter for the new source stack, reading replies as zebrad
/// sends them.
fn rpc_adapter(
    common: &crate::config::CommonBackendConfig,
) -> Result<zaino_source_zcash_rpc::ZcashRpcAdapter, BlockchainSourceError> {
    Ok(zaino_source_zebra::rpc_adapter(rpc_client(common)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A height above the protocol maximum is refused at the boundary.
    #[test]
    fn heights_reject_impossible_values() {
        assert_eq!(
            domain_height(1_234_567)
                .map(u32::from)
                .expect("within range"),
            1_234_567
        );
        assert!(domain_height(u32::MAX).is_err());
    }

    /// A domain rejection and a transport fault are different facts. The
    /// scaffolding error cannot represent the distinction structurally, so it
    /// must survive in the message rather than being flattened away.
    #[test]
    fn error_flattening_keeps_the_failure_kind() {
        let domain: QueryError<zaino_source::GetChainTipError> =
            QueryError::Domain(zaino_source::GetChainTipError::NotReady);
        let transport: QueryError<zaino_source::GetChainTipError> = QueryError::NonDomain(
            zaino_source::NonDomainError::new(zaino_source::FailureMode::Connection, "refused"),
        );

        assert!(err(domain).to_string().contains("rejected"));
        assert!(err(transport).to_string().contains("unreachable"));
    }

    /// Reads differently forwards and backwards, so a mirrored result cannot
    /// pass by coincidence.
    const ASYMMETRIC: [u8; 32] = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
        0xee, 0x01,
    ];

    /// The inverse of the display-order parsers under test.
    ///
    /// Production rendering lives in the serving layer's wire module with the
    /// responses that need it; what remains here is parsing. This oracle stays
    /// so the parsers can still be pinned against a round trip.
    fn display_hex(mut bytes: [u8; 32]) -> String {
        bytes.reverse();
        hex::encode(bytes)
    }

    /// A txid arrives from the interface in display order and must reach the
    /// port in internal order. Paired with `display_hex`, the two must compose
    /// to the identity — otherwise a lookup silently queries a different
    /// transaction.
    #[test]
    fn txid_parse_and_render_are_inverses() {
        let mut display_order = ASYMMETRIC;
        display_order.reverse();
        let as_written = hex::encode(display_order);

        let parsed = parse_display_txid(&as_written).expect("valid txid");
        assert_eq!(
            <[u8; 32]>::from(parsed),
            ASYMMETRIC,
            "parsing did not restore internal order"
        );
        assert_eq!(
            display_hex(<[u8; 32]>::from(parsed)),
            as_written,
            "render(parse(x)) must be x"
        );
    }

    #[test]
    fn txid_parse_rejects_malformed_input() {
        assert!(parse_display_txid("not hex").is_err());
        assert!(
            parse_display_txid("aabb").is_err(),
            "a short txid must not be silently accepted"
        );
    }

    /// The pool enums share a name and their variants but not a role, so the
    /// mapping is written by hand. A transposition here would query the wrong
    /// pool's subtrees and return plausible-looking wrong roots.
    #[test]
    fn shielded_pool_mapping_is_not_transposed() {
        use crate::chain_index::ShieldedPool as Ours;
        use zaino_primitives::types::ShieldedPool as Theirs;

        assert_eq!(Ours::Sapling.to_domain(), Theirs::Sapling);
        assert_eq!(Ours::Orchard.to_domain(), Theirs::Orchard);
        assert_eq!(Ours::Ironwood.to_domain(), Theirs::Ironwood);
    }

    /// Block identifiers arrive from the interface in display order. This is
    /// the same contract as `parse_display_txid`, and the two must not drift
    /// apart.
    #[test]
    fn hash32_parse_restores_internal_order() {
        let mut display_order = ASYMMETRIC;
        display_order.reverse();

        let parsed = parse_display_hash32(&hex::encode(display_order)).expect("valid hash");

        assert_eq!(parsed, ASYMMETRIC);
        assert_eq!(display_hex(parsed), hex::encode(display_order));
    }

    #[test]
    fn hash32_parse_rejects_wrong_length() {
        assert!(parse_display_hash32("aabbcc").is_err());
        assert!(parse_display_hash32("zz".repeat(32).as_str()).is_err());
    }
}

#[cfg(test)]
mod error_source_chain {
    use super::*;

    /// A serving layer recovers legacy-compatible RPC error codes by
    /// downcast-walking [`std::error::Error::source`] chains. This boundary
    /// must therefore
    /// preserve the typed cause instead of flattening it to a string, or
    /// failures surface as generic internal errors rather than the legacy codes
    /// lightwalletd-family clients key on.
    /// A domain rejection must reach the serving layer with a code too.
    ///
    /// Before the adapters learned to distinguish a missing object from an
    /// unreachable node, "block not found" arrived here as a `Fetch` error
    /// carrying `-8`, and the serving layer recovered that code. Now it arrives as
    /// a `Domain` rejection, so this boundary must attach the code itself —
    /// otherwise the reclassification would silently downgrade every
    /// not-found response to a generic internal error.
    #[test]
    fn domain_rejection_carries_a_legacy_code_through_source() {
        let flattened = err::<zaino_source::GetBlockError, zaino_source::NonDomainError>(
            QueryError::Domain(zaino_source::GetBlockError::HeightNotFound(
                zaino_primitives::types::Height::try_from(42u32).expect("valid height"),
            )),
        );

        let mut current: Option<&(dyn std::error::Error + 'static)> = Some(&flattened);
        let mut code = None;
        while let Some(source) = current {
            if let Some(rejection) = source.downcast_ref::<crate::error::LegacyRpcError>() {
                code = Some(rejection.code);
                break;
            }
            current = source.source();
        }

        assert_eq!(
            code,
            Some(zebra_rpc::server::error::LegacyCode::InvalidParameter as i64),
            "a domain rejection must stay downcastable to its legacy code"
        );
    }

    /// Recovers the legacy code a rejection carries, the way a serving layer
    /// does: by walking the `source()` chain and downcasting.
    fn recovered_code(error: &BlockchainSourceError) -> Option<i64> {
        std::iter::successors(Some(error as &(dyn std::error::Error + 'static)), |error| {
            error.source()
        })
        .find_map(|source| {
            source
                .downcast_ref::<crate::error::LegacyRpcError>()
                .map(|rejection| rejection.code)
        })
    }

    /// the legacy full node answers "no spend on record" with `-5 Unable to get spent info`,
    /// and that pair is what a client matches on. Neither this method's first
    /// rewrite nor the connector it replaced actually served it — the former
    /// reported `-8`, the latter consumed the code into a typed error and
    /// served a generic internal error. Both lost the only part the client uses.
    #[test]
    fn an_unspent_output_reports_the_legacy_full_nodes_own_code() {
        let rejected = spent_info_err::<zaino_source::NonDomainError>(QueryError::Domain(
            zaino_source::GetSpentInfoError::NotSpent,
        ));

        assert_eq!(
            recovered_code(&rejected),
            Some(zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey as i64)
        );
        assert!(
            rejected.to_string().contains("Unable to get spent info"),
            "the legacy full node's message travels with its code: {rejected}"
        );
    }

    /// `getspentinfo` is legacy-only, so a zebrad-backed deployment cannot
    /// answer it at all. That has to stay distinguishable from "unspent" all
    /// the way to the wire — served as `-5`, it would tell every client that
    /// every output is unspent.
    #[test]
    fn an_unsupported_method_does_not_masquerade_as_unspent() {
        let rejected = spent_info_err::<zaino_source::NonDomainError>(QueryError::Domain(
            zaino_source::GetSpentInfoError::Unsupported,
        ));

        assert_eq!(
            recovered_code(&rejected),
            Some(crate::error::LegacyRpcError::METHOD_NOT_FOUND)
        );
        assert_ne!(
            recovered_code(&rejected),
            Some(zebra_rpc::server::error::LegacyCode::InvalidAddressOrKey as i64)
        );
    }

    /// A validator without `getstandardfee` keeps `-32601` and reaches the
    /// chain index as `Unimplemented`, which gRPC serves as `UNIMPLEMENTED`.
    #[test]
    fn an_unsupported_standard_fee_is_unimplemented() {
        let rejected = standard_fee_err::<zaino_source::NonDomainError>(QueryError::Domain(
            zaino_source::GetStandardFeeError::Unsupported,
        ));

        assert_eq!(
            recovered_code(&rejected),
            Some(crate::error::LegacyRpcError::METHOD_NOT_FOUND)
        );
        assert_eq!(
            crate::error::ChainIndexError::backing_validator(rejected).kind(),
            crate::error::ChainIndexErrorKind::Unimplemented
        );
    }

    /// Any other `getstandardfee` failure stays an internal error.
    #[test]
    fn a_standard_fee_not_ready_stays_internal() {
        let rejected = standard_fee_err::<zaino_source::NonDomainError>(QueryError::Domain(
            zaino_source::GetStandardFeeError::NotReady,
        ));

        assert_eq!(
            crate::error::ChainIndexError::backing_validator(rejected).kind(),
            crate::error::ChainIndexErrorKind::InternalServerError
        );
    }

    /// A transport fault is not an answer about the outpoint, so it must keep
    /// the typed `NonDomainError` rather than acquiring a legacy code that would
    /// tell the client something about the output.
    #[test]
    fn a_transport_fault_on_spent_info_stays_a_fetch_failure() {
        let rejected = spent_info_err(QueryError::NonDomain(zaino_source::NonDomainError::new(
            zaino_source::FailureMode::Connection,
            "refused",
        )));

        assert_eq!(recovered_code(&rejected), None);
        assert!(rejected.to_string().contains("unreachable"));
    }

    #[tokio::test]
    async fn transport_error_stays_reachable_through_source() {
        // Port 1 refuses connections, so the request fails at the transport
        // layer without contacting any validator.
        let source = ZebraValidatorSource::rpc_only(
            "127.0.0.1:1",
            None,
            zebra_chain::parameters::Network::new_regtest(Default::default()),
        )
        .expect("client construction is network-free");

        let error = err(
            zaino_source::OneShotGetChainTip::get_chain_tip(source.source())
                .await
                .expect_err("a request to a closed port must fail"),
        );

        let reached = std::iter::successors(
            Some(&error as &(dyn std::error::Error + 'static)),
            |error| error.source(),
        )
        .any(|error| {
            error
                .downcast_ref::<zaino_source::NonDomainError>()
                .is_some()
        });

        assert!(
            reached,
            "the typed NonDomainError must stay reachable via the source() chain; \
             stringifying it strips the FailureMode the serve layer recovers"
        );
    }
}
