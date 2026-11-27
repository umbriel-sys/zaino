//! JSON-RPC 2.0 client: HTTP transport, auth, and call orchestration.
//!
//! Composes [`envelope`] (request/response serialization) and
//! [`retry`] (retry policy). This module owns the HTTP side only.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::sync::{Semaphore, SemaphorePermit};

use crate::envelope::{self, ResponseOutcome};
use crate::error::RpcError;
use crate::retry;

/// Request timeout for the few JSON-RPC methods that are inherently heavy on the
/// validator, overriding [`RpcClientConfig::request_timeout`].
///
/// `getrawmempool verbose` is the motivating case: Zebra services it by loading
/// full transactions and aggregating descendant stats over the whole mempool, so
/// on a busy chain it legitimately takes longer than the default. Failing it
/// leaves a consumer's mempool marked incomplete precisely when the chain is
/// busiest, so a slow answer is far better than an error.
///
/// This **must** stay above [`RpcClientConfig::request_timeout`]'s default, or
/// the override buys nothing and the failure above happens anyway. Four times
/// the default: enough that a heavy call has real room, bounded enough that an
/// unreachable validator is still noticed in seconds rather than minutes. The
/// caller that waits on it is a poll loop which serves its previous set
/// meanwhile, so a slow answer costs staleness, not availability.
pub const HEAVY_METHOD_TIMEOUT: Duration = Duration::from_secs(120);

/// Maximum response body this client will buffer, in bytes.
///
/// Every response is deserialized into memory, so without a cap a validator that
/// is compromised, misconfigured, or simply impersonated can drive the process
/// out of memory with one reply — and the largest legitimate responses (a full
/// block, a verbose mempool listing) are orders of magnitude below this, so the
/// cap has generous headroom before it can affect healthy operation.
pub const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;

/// The default for [`RpcClientConfig::max_in_flight`].
///
/// zebrad serves 100 requests at once and answers every other with HTTP 429.
/// 64 keeps one client's whole load under that with room for the
/// validator's other clients.
pub const DEFAULT_MAX_IN_FLIGHT: usize = 64;

/// Configuration for [`RpcClient`].
pub struct RpcClientConfig {
    /// RPC endpoint URL.
    pub url: String,
    /// Basic auth credentials (username, password). Optional.
    pub auth: Option<(String, String)>,
    /// HTTP connect timeout.
    pub connect_timeout: Duration,
    /// HTTP request timeout.
    pub request_timeout: Duration,
    /// Max retries of a call the validator was too busy to take (HTTP 429
    /// or 503).
    pub max_retries: u32,
    /// Delay between retries.
    pub retry_delay: Duration,
    /// The most requests this client has in flight at once; the rest wait
    /// their turn, for no longer than a request may take. At least 1.
    pub max_in_flight: usize,
}

impl Default for RpcClientConfig {
    fn default() -> Self {
        Self {
            url: "http://127.0.0.1:8232".to_string(),
            auth: None,
            connect_timeout: Duration::from_secs(2),
            request_timeout: Duration::from_secs(30),
            max_retries: 5,
            retry_delay: Duration::from_millis(500),
            max_in_flight: DEFAULT_MAX_IN_FLIGHT,
        }
    }
}

/// A JSON-RPC 2.0 client.
///
/// Handles HTTP transport, JSON-RPC envelope, authentication, a bound on
/// requests in flight, and retry of a call the validator was too busy to
/// take. Returns raw `serde_json::Value` results — response parsing is the
/// adapter crate's responsibility.
pub struct RpcClient {
    url: String,
    client: reqwest::Client,
    auth: Option<(String, String)>,
    id_counter: AtomicI64,
    request_timeout: Duration,
    max_retries: u32,
    retry_delay: Duration,
    max_in_flight: usize,
    in_flight: Semaphore,
}

impl RpcClient {
    /// Create a new client from config.
    pub fn new(config: RpcClientConfig) -> Result<Self, RpcError> {
        // This is a TLS boundary, so it must install the process-level rustls
        // `CryptoProvider` before building the client. The workspace enables
        // reqwest's `rustls-no-provider` feature, which never auto-selects one,
        // so a client built without this panics with "No provider set" the
        // moment it is constructed. First-install-wins, so an embedder that
        // installed its own provider keeps it.
        crate::crypto::ensure_default_crypto_provider();

        let client = reqwest::Client::builder()
            .connect_timeout(config.connect_timeout)
            .timeout(config.request_timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let max_in_flight = config.max_in_flight.max(1);
        Ok(Self {
            url: config.url,
            client,
            auth: config.auth,
            id_counter: AtomicI64::new(0),
            request_timeout: config.request_timeout,
            max_retries: config.max_retries,
            retry_delay: config.retry_delay,
            max_in_flight,
            in_flight: Semaphore::new(max_in_flight),
        })
    }

    /// Make a JSON-RPC call.
    ///
    /// Returns the `result` field from the response as a raw `Value`.
    /// A validator too busy to take the call (HTTP 429 or 503) is asked
    /// again up to `max_retries` times, then the call fails with
    /// [`RpcError::Busy`]. Waits its turn among `max_in_flight` requests,
    /// and fails with [`RpcError::Saturated`] if none comes within the
    /// request timeout. An error object is returned as [`RpcError::Rpc`]
    /// whatever HTTP status carried it, and is never retried.
    ///
    /// - `method` is `&'static str` because it is also a metric label: cardinality
    ///   bounded to the compiled-in method set, no per-request alloc
    pub async fn call(&self, method: &'static str, params: Vec<Value>) -> Result<Value, RpcError> {
        self.call_with_timeout(method, params, None).await
    }

    /// [`Self::call`] with an optional per-request timeout overriding
    /// [`RpcClientConfig::request_timeout`].
    ///
    /// The client-wide timeout suits the small, fast RPCs that dominate normal
    /// traffic, but a few methods are inherently heavy on the validator (see
    /// [`HEAVY_METHOD_TIMEOUT`]). Timing those out turns a slow-but-healthy
    /// validator into a hard error. Overriding per request keeps the tight
    /// default everywhere else.
    pub async fn call_with_timeout(
        &self,
        method: &'static str,
        params: Vec<Value>,
        timeout: Option<Duration>,
    ) -> Result<Value, RpcError> {
        let mut attempt = 0u32;

        loop {
            attempt = attempt.saturating_add(1);
            let id = self.id_counter.fetch_add(1, Ordering::Relaxed);

            let body = envelope::build_request(method, params.clone(), id);

            // The slot is held for the request only, never across a retry sleep
            let (started, sent) = {
                let _slot = match self.slot(timeout.unwrap_or(self.request_timeout)).await {
                    Ok(slot) => slot,
                    Err(error) => {
                        Self::record_failure(method, "saturated");
                        return Err(error);
                    }
                };
                // Per attempt, excluding the wait for a slot and the retry
                // sleeps: folding those in makes a saturated validator look
                // like a slow one
                let started = std::time::Instant::now();
                (started, self.send_http(&body, timeout).await)
            };

            let response_bytes = match sent {
                Ok(bytes) => bytes,
                Err(RpcError::Status(status)) if retry::is_busy_status(status) => {
                    if retry::should_retry(attempt, self.max_retries) {
                        Self::record_failure(method, "retried");
                        tokio::time::sleep(self.retry_delay).await;
                        continue;
                    }
                    Self::record_failure(method, "busy");
                    return Err(RpcError::Busy {
                        attempts: attempt,
                        status,
                    });
                }
                Err(error) => {
                    // No JSON-RPC code exists for a refused or timed-out connection
                    Self::record_failure(method, "transport_error");
                    return Err(error);
                }
            };
            let outcome = match envelope::parse_response(&response_bytes) {
                Ok(outcome) => outcome,
                Err(error) => {
                    // Malformed envelope = transport failure (no answer to the question)
                    Self::record_failure(method, "transport_error");
                    return Err(error);
                }
            };

            metrics::histogram!(
                crate::metric_names::RPC_OUTBOUND_DURATION_SECONDS,
                crate::metric_names::RPC_METHOD => method,
            )
            .record(started.elapsed().as_secs_f64());

            match outcome {
                ResponseOutcome::Success(value) => return Ok(value),
                ResponseOutcome::RpcError { code, message } => {
                    // The validator's considered reply, never asked again: no
                    // code is a busy signal (zebrad and zakura answer -1 for
                    // any failed request, invalid parameters among them)
                    Self::record_failure(method, "rpc_error");
                    return Err(RpcError::Rpc { code, message });
                }
            }
        }
    }

    /// A place among the `max_in_flight` requests, waited for no longer
    /// than `wait`.
    async fn slot(&self, wait: Duration) -> Result<SemaphorePermit<'_>, RpcError> {
        match tokio::time::timeout(wait, self.in_flight.acquire()).await {
            Ok(Ok(slot)) => Ok(slot),
            // the semaphore is never closed; were it, no slot would ever come
            Ok(Err(_)) | Err(_) => Err(RpcError::Saturated {
                waited: wait,
                max_in_flight: self.max_in_flight,
            }),
        }
    }

    /// - Success is not counted: attempt volume is the duration histogram's `_count`
    #[inline]
    fn record_failure(method: &'static str, outcome: &'static str) {
        metrics::counter!(
            crate::metric_names::RPC_OUTBOUND_ERRORS_TOTAL,
            crate::metric_names::RPC_METHOD => method,
            crate::metric_names::RPC_OUTCOME => outcome,
        )
        .increment(1);
    }

    /// Send an HTTP POST with the JSON body, return raw response bytes.
    async fn send_http(
        &self,
        body: &Value,
        timeout: Option<Duration>,
    ) -> Result<Vec<u8>, RpcError> {
        let mut request = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(serde_json::to_string(body)?);

        if let Some((ref user, ref pass)) = self.auth {
            request = request.basic_auth(user, Some(pass));
        }

        if let Some(timeout) = timeout {
            request = request.timeout(timeout);
        }

        let response = request.send().await?;
        let status = response.status().as_u16();

        if (200..300).contains(&status) {
            return read_body_capped(response, MAX_RESPONSE_BYTES).await;
        }
        // A busy or auth status is the answer whatever its body says (zebrad's
        // 401 carries an error object whose code is the status)
        if retry::is_busy_status(status) || matches!(status, 401 | 403) {
            return Err(RpcError::Status(status));
        }
        // Any other status may still carry the validator's error object:
        // zebrad and zakura answer an unreadable or oversized request so
        match read_body_capped(response, MAX_ERROR_BODY_BYTES).await {
            Ok(body)
                if matches!(
                    envelope::parse_response(&body),
                    Ok(ResponseOutcome::RpcError { .. })
                ) =>
            {
                Ok(body)
            }
            _ => Err(RpcError::Status(status)),
        }
    }
}

/// The most of a failed response's body that is read to look for an error
/// object.
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

/// Reads a response body into memory, abandoning it if it exceeds `max`.
///
/// Chunk-wise rather than [`reqwest::Response::bytes`], which would buffer the
/// whole body before any size could be checked — the point is to never allocate
/// the oversized body in the first place.
async fn read_body_capped(
    mut response: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, RpcError> {
    // A truthful Content-Length lets us reject before reading a single chunk;
    // an absent or lying one is caught by the running total below.
    if response
        .content_length()
        .is_some_and(|len| len > max as u64)
    {
        return Err(RpcError::ResponseBodyTooLarge { max });
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max {
            return Err(RpcError::ResponseBodyTooLarge { max });
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::*;

    /// An HTTP status line and body.
    type Reply = (&'static str, &'static str);

    const OK: Reply = ("200 OK", r#"{"jsonrpc":"2.0","id":0,"result":true}"#);
    /// zebrad's and zakura's answer past 100 requests in flight.
    const TOO_MANY: Reply = (
        "429 Too Many Requests",
        "Too many connections. Please try again later.",
    );
    const UNAVAILABLE: Reply = ("503 Service Unavailable", "");
    const BROKEN: Reply = ("500 Internal Server Error", "broken");
    /// zebrad's and zakura's answer to parameters they cannot read.
    const INVALID_PARAMS: Reply = (
        "200 OK",
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-1,"message":"Invalid params"}}"#,
    );
    /// zebrad's and zakura's answer to a body that is not JSON.
    const PARSE_ERROR: Reply = (
        "400 Bad Request",
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}"#,
    );
    /// A server that puts its error objects on a 500.
    const NOT_FOUND_ON_500: Reply = (
        "500 Internal Server Error",
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-8,"message":"Block not found"}}"#,
    );
    const INTERNAL_ON_500: Reply = (
        "500 Internal Server Error",
        r#"{"jsonrpc":"2.0","id":0,"error":{"code":-32603,"message":"the database is closed"}}"#,
    );
    /// zebrad's answer to wrong credentials: an error object whose code is
    /// the status.
    const UNAUTHORIZED: Reply = (
        "401 Unauthorized",
        r#"{"jsonrpc":"2.0","id":null,"error":{"code":401,"message":"unauthorized"}}"#,
    );

    /// A local HTTP server standing in for a validator.
    struct Validator {
        url: String,
        /// Requests received.
        seen: Arc<AtomicUsize>,
        /// The most requests it held at once.
        peak: Arc<AtomicUsize>,
    }

    impl Validator {
        /// Holds each request for `hold`, then answers it with the status
        /// `answer` gives for the number of requests before it.
        async fn start(hold: Duration, answer: fn(usize) -> Reply) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
            let url = format!("http://{}", listener.local_addr().expect("an address"));
            let (seen, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
            let held = Arc::new(AtomicUsize::new(0));
            let counters = (Arc::clone(&seen), Arc::clone(&peak));
            tokio::spawn(async move {
                while let Ok((stream, _)) = listener.accept().await {
                    let (seen, peak) = (Arc::clone(&counters.0), Arc::clone(&counters.1));
                    let held = Arc::clone(&held);
                    tokio::spawn(async move {
                        let mut stream = stream;
                        while read_request(&mut stream).await {
                            let before = seen.fetch_add(1, Ordering::SeqCst);
                            peak.fetch_max(
                                held.fetch_add(1, Ordering::SeqCst) + 1,
                                Ordering::SeqCst,
                            );
                            tokio::time::sleep(hold).await;
                            held.fetch_sub(1, Ordering::SeqCst);
                            let (line, body) = answer(before);
                            let response = format!(
                                "HTTP/1.1 {line}\r\ncontent-length: {}\r\n\r\n{body}",
                                body.len()
                            );
                            if stream.write_all(response.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                    });
                }
            });
            Self { url, seen, peak }
        }

        fn client(&self, config: RpcClientConfig) -> RpcClient {
            RpcClient::new(RpcClientConfig {
                url: self.url.clone(),
                retry_delay: Duration::from_millis(1),
                ..config
            })
            .expect("builds")
        }
    }

    /// Reads one request off `stream`; false once the client has closed it.
    async fn read_request(stream: &mut TcpStream) -> bool {
        let mut request = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            let head_end = request.windows(4).position(|bytes| bytes == b"\r\n\r\n");
            if let Some(head_end) = head_end {
                let head = String::from_utf8_lossy(&request[..head_end]).to_ascii_lowercase();
                let body_len: usize = head
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length:"))
                    .map(|len| len.trim().parse().expect("a length"))
                    .unwrap_or(0);
                if request.len() >= head_end + 4 + body_len {
                    return true;
                }
            }
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return false,
                Ok(read) => request.extend_from_slice(&chunk[..read]),
            }
        }
    }

    /// A validator at its request limit answers 429: the call is asked again
    /// and succeeds once the validator has room.
    #[tokio::test]
    async fn a_429_is_retried_until_the_validator_has_room() {
        let validator = Validator::start(Duration::ZERO, |before| match before {
            0 => TOO_MANY,
            1 => UNAVAILABLE,
            _ => OK,
        })
        .await;
        let client = validator.client(RpcClientConfig::default());
        let answer = client.call("getinfo", vec![]).await.expect("answered");
        assert_eq!(answer, Value::Bool(true));
        assert_eq!(validator.seen.load(Ordering::SeqCst), 3);
    }

    /// A validator that never has room is given up on after the retries, and
    /// the failure says it was busy, how often it was asked, and its status.
    #[tokio::test]
    async fn a_validator_that_stays_busy_is_the_error_after_the_retries() {
        let validator = Validator::start(Duration::ZERO, |_| TOO_MANY).await;
        let client = validator.client(RpcClientConfig {
            max_retries: 3,
            ..RpcClientConfig::default()
        });
        let error = client.call("getinfo", vec![]).await.expect_err("gives up");
        assert_eq!(
            error.to_string(),
            "the validator stayed busy (http status 429) through 4 attempt(s)"
        );
        assert_eq!(
            zaino_source::NonDomainError::from(error).mode,
            zaino_source::FailureMode::Busy
        );
        assert_eq!(validator.seen.load(Ordering::SeqCst), 4);
    }

    /// Another failing status is not retried, and without an error object
    /// it is the error.
    #[tokio::test]
    async fn another_status_is_not_retried() {
        let validator = Validator::start(Duration::ZERO, |_| BROKEN).await;
        let client = validator.client(RpcClientConfig::default());
        let error = client.call("getinfo", vec![]).await.expect_err("fails");
        assert!(matches!(error, RpcError::Status(500)), "{error}");
        assert_eq!(validator.seen.load(Ordering::SeqCst), 1);
    }

    /// `-1` is the validator's answer to any failed request, not a busy
    /// signal: it is returned at once.
    #[tokio::test]
    async fn an_error_object_is_never_asked_again() {
        let validator = Validator::start(Duration::ZERO, |_| INVALID_PARAMS).await;
        let client = validator.client(RpcClientConfig::default());
        let error = client.call("getblock", vec![]).await.expect_err("refused");
        assert!(
            matches!(&error, RpcError::Rpc { code: -1, message } if message == "Invalid params"),
            "{error}"
        );
        assert_eq!(validator.seen.load(Ordering::SeqCst), 1);
    }

    /// An error object keeps its code whatever HTTP status carried it.
    #[tokio::test]
    async fn an_error_object_on_a_failing_status_keeps_its_code() {
        let validator = Validator::start(Duration::ZERO, |_| PARSE_ERROR).await;
        let client = validator.client(RpcClientConfig::default());
        let error = client.call("getinfo", vec![]).await.expect_err("refused");
        assert!(
            matches!(&error, RpcError::Rpc { code: -32700, message } if message == "Parse error"),
            "{error}"
        );
    }

    /// The same on a 5xx: the error object is the validator's answer, an
    /// internal error with its message included.
    #[tokio::test]
    async fn an_error_object_on_a_5xx_keeps_its_code_and_message() {
        let validator = Validator::start(Duration::ZERO, |before| match before {
            0 => NOT_FOUND_ON_500,
            _ => INTERNAL_ON_500,
        })
        .await;
        let client = validator.client(RpcClientConfig::default());
        let error = client.call("getblock", vec![]).await.expect_err("refused");
        assert!(
            matches!(&error, RpcError::Rpc { code: -8, message } if message == "Block not found"),
            "{error}"
        );
        let error = client.call("getblock", vec![]).await.expect_err("refused");
        assert!(
            matches!(
                &error,
                RpcError::Rpc { code: -32603, message } if message == "the database is closed"
            ),
            "{error}"
        );
        assert_eq!(validator.seen.load(Ordering::SeqCst), 2);
    }

    /// A refused credential is the status, not the error object zebrad sends
    /// with it: the source layer reads the status as an auth failure.
    #[tokio::test]
    async fn refused_credentials_are_an_auth_failure() {
        let validator = Validator::start(Duration::ZERO, |_| UNAUTHORIZED).await;
        let client = validator.client(RpcClientConfig::default());
        let error = client.call("getinfo", vec![]).await.expect_err("refused");
        assert!(matches!(error, RpcError::Status(401)), "{error}");
        assert_eq!(
            zaino_source::NonDomainError::from(error).mode,
            zaino_source::FailureMode::Auth
        );
    }

    /// A failed connection's error does not carry the validator's address:
    /// the text reaches Zaino's clients.
    #[tokio::test]
    async fn a_connection_failure_does_not_name_the_validators_address() {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
        let address = listener.local_addr().expect("an address");
        drop(listener);
        let client = RpcClient::new(RpcClientConfig {
            url: format!("http://{address}"),
            ..RpcClientConfig::default()
        })
        .expect("builds");
        let error = client.call("getinfo", vec![]).await.expect_err("refused");
        assert!(matches!(error, RpcError::Http(_)), "{error}");
        let text = zaino_source::NonDomainError::from(error).to_string();
        assert!(!text.contains("127.0.0.1"), "{text}");
        assert!(!text.contains(&address.port().to_string()), "{text}");
    }

    /// However many calls are made at once, the validator never holds more
    /// than `max_in_flight` of them, and every call is answered.
    #[tokio::test]
    async fn no_more_than_max_in_flight_requests_reach_the_validator() {
        let validator = Validator::start(Duration::from_millis(20), |_| OK).await;
        let client = Arc::new(validator.client(RpcClientConfig {
            max_in_flight: 4,
            ..RpcClientConfig::default()
        }));
        let mut calls = tokio::task::JoinSet::new();
        for _ in 0..32 {
            let client = Arc::clone(&client);
            calls.spawn(async move { client.call("getinfo", vec![]).await });
        }
        while let Some(call) = calls.join_next().await {
            call.expect("joins").expect("answered");
        }
        assert_eq!(validator.seen.load(Ordering::SeqCst), 32);
        assert_eq!(validator.peak.load(Ordering::SeqCst), 4);
    }

    /// A call that gets no slot within its timeout fails as saturated,
    /// without reaching the validator; the call holding the slot is unharmed.
    #[tokio::test]
    async fn a_call_that_gets_no_slot_in_time_fails_as_saturated() {
        let validator = Validator::start(Duration::from_millis(500), |_| OK).await;
        let client = Arc::new(validator.client(RpcClientConfig {
            max_in_flight: 1,
            ..RpcClientConfig::default()
        }));
        let holder = Arc::clone(&client);
        let holding = tokio::spawn(async move { holder.call("getinfo", vec![]).await });
        while validator.seen.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        let error = client
            .call_with_timeout("getinfo", vec![], Some(Duration::from_millis(50)))
            .await
            .expect_err("no slot");
        assert!(
            matches!(
                error,
                RpcError::Saturated {
                    max_in_flight: 1,
                    ..
                }
            ),
            "{error}"
        );
        assert_eq!(validator.seen.load(Ordering::SeqCst), 1);
        holding.await.expect("joins").expect("answered");
    }

    /// A limit of zero would let nothing through: it is one.
    #[tokio::test]
    async fn a_limit_of_zero_is_one() {
        let validator = Validator::start(Duration::ZERO, |_| OK).await;
        let client = validator.client(RpcClientConfig {
            max_in_flight: 0,
            ..RpcClientConfig::default()
        });
        client.call("getinfo", vec![]).await.expect("answered");
    }

    /// The whole point of [`HEAVY_METHOD_TIMEOUT`] is headroom over the
    /// client-wide default. Equal values make the override a no-op while
    /// reading, at every call site, as though heavy methods were handled — the
    /// state this constant shipped in before it was caught in review.
    #[test]
    fn heavy_method_timeout_actually_grants_headroom() {
        assert!(
            HEAVY_METHOD_TIMEOUT > RpcClientConfig::default().request_timeout,
            "HEAVY_METHOD_TIMEOUT ({HEAVY_METHOD_TIMEOUT:?}) must exceed the default \
             request_timeout ({:?}), or overriding with it changes nothing",
            RpcClientConfig::default().request_timeout,
        );
    }
}
