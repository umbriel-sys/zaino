//! Admin surface: `/metrics`, `/livez`, `/readyz`, on a runtime of its own.
//!
//! - Own thread + current-thread runtime: a probe answered from the saturated serving runtime
//!   measures its queue, and a timed-out liveness probe gets the pod killed

use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use http_body_util::Full;
use hyper::{body::Bytes, server::conn::http1, service::service_fn, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use metrics_exporter_prometheus::PrometheusHandle;
use tracing::{error, info, warn};
use zaino_chain_store_service::Serviceability;
use zaino_component::{ComponentStatus, Lifecycle};
use zaino_indexes::IndexId;

use crate::error::IndexerError;

/// Bounds `AtomicBucket` growth between scrapes (`render` drains itself; this covers no scraper)
const UPKEEP_INTERVAL: Duration = Duration::from_secs(5);

/// Concurrent admin connections; the rest wait in the accept backlog
///
/// - Unbounded `spawn` per connection = fd exhaustion of the whole process (may bind non-private)
const MAX_CONNECTIONS: usize = 32;

/// Heartbeat age past which `/livez` fails
///
/// - Indexer republishes every 100ms; a wedged runtime stops while this thread keeps answering
const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(30);

static HEARTBEAT: Mutex<Option<Instant>> = Mutex::new(None);

/// What `/readyz` answers from: every component and every index, as the
/// indexer loop last saw them.
#[derive(Debug, Clone)]
pub(crate) struct Readiness {
    /// The components requests are answered through: all must be ready.
    pub(crate) serving: Vec<ComponentStatus>,
    /// The store and its background sync, which build while the validator
    /// answers: they must be live.
    pub(crate) building: Vec<ComponentStatus>,
    /// How far each index serves.
    pub(crate) indexes: Vec<(IndexId, Serviceability)>,
    /// Whether every answer is Zaino's own yet.
    pub(crate) local: bool,
}

impl Readiness {
    /// Whether the daemon answers requests now.
    fn is_ready(&self) -> bool {
        self.serving
            .iter()
            .all(|status| status.lifecycle == Lifecycle::Ready && status.is_live())
            && self.building.iter().all(ComponentStatus::is_live)
    }

    fn to_json(&self) -> serde_json::Value {
        let component = |status: &ComponentStatus| {
            serde_json::json!({
                "name": status.name.to_string(),
                "lifecycle": format!("{:?}", status.lifecycle),
                "health": format!("{:?}", status.health),
                "reason": status.reason,
            })
        };
        let index = |(id, state): &(IndexId, Serviceability)| {
            let (state, height) = match state {
                Serviceability::Absent => ("absent", None),
                Serviceability::Building(height) => ("building", *height),
                Serviceability::ToHeight(height) => ("built", Some(*height)),
            };
            serde_json::json!({
                "name": id.name(),
                "state": state,
                "height": height.map(u32::from),
            })
        };
        serde_json::json!({
            "ready": self.is_ready(),
            "local": self.local,
            "components": self
                .serving
                .iter()
                .chain(&self.building)
                .map(component)
                .collect::<Vec<_>>(),
            "indexes": self.indexes.iter().map(index).collect::<Vec<_>>(),
        })
    }
}

static READINESS: Mutex<Option<Readiness>> = Mutex::new(None);

/// Called by the indexer loop every tick
pub(crate) fn heartbeat(readiness: Readiness) {
    // Poison-tolerant: each lock only guards a swap
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    *READINESS.lock().unwrap_or_else(PoisonError::into_inner) = Some(readiness);
}

/// Supervisor restart: back to "still starting" (the respawn stops the loop, may outlast 30s)
pub(crate) fn clear_heartbeat() {
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner) = None;
    *READINESS.lock().unwrap_or_else(PoisonError::into_inner) = None;
}

fn last_readiness() -> Option<Readiness> {
    READINESS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn last_heartbeat() -> Option<Instant> {
    *HEARTBEAT.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Never beaten = still starting (k8s covers that window with its own startup delay)
fn is_fresh(heartbeat: Option<Instant>) -> bool {
    heartbeat.is_none_or(|at| at.elapsed() < HEARTBEAT_MAX_AGE)
}

/// Binds the admin endpoint, which `metrics::init` calls before it installs the recorder so that no sample is ever recorded without a listener to drain it.
pub(crate) fn bind(endpoint: SocketAddr) -> Result<std::net::TcpListener, IndexerError> {
    let listener = std::net::TcpListener::bind(endpoint).map_err(|e| {
        IndexerError::MetricsError(format!("admin endpoint {endpoint} failed to bind: {e}"))
    })?;
    listener
        .set_nonblocking(true)
        .map_err(|e| IndexerError::MetricsError(format!("admin endpoint {endpoint}: {e}")))?;
    Ok(listener)
}

/// Serves a bound admin listener on a thread and runtime of its own.
pub(crate) fn spawn(
    listener: std::net::TcpListener,
    handle: PrometheusHandle,
) -> Result<(), IndexerError> {
    // the thread's start-up outcome: a failure fails zainod's start-up
    let (started, starting) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("zaino-admin".to_string())
        .spawn(move || {
            // set however the thread ends, a panic included
            let _stopped = Stopped;
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => return report(&started, Err(format!("admin runtime: {e}"))),
            };
            runtime.block_on(async move {
                let listener = match tokio::net::TcpListener::from_std(listener) {
                    Ok(listener) => listener,
                    Err(e) => return report(&started, Err(format!("admin listener: {e}"))),
                };
                report(&started, Ok(()));
                serve(listener, handle).await;
            });
        })
        .map_err(|e| IndexerError::MetricsError(format!("failed to spawn admin thread: {e}")))?;
    match starting.recv() {
        Ok(Ok(())) => Ok(()),
        Ok(Err(failure)) => Err(IndexerError::MetricsError(failure)),
        Err(_) => Err(IndexerError::MetricsError(
            "the admin thread ended before it started serving".to_string(),
        )),
    }
}

/// The admin thread's start-up outcome to the thread that spawned it.
fn report(started: &std::sync::mpsc::Sender<Result<(), String>>, outcome: Result<(), String>) {
    if started.send(outcome).is_err() {
        error!("the admin endpoint's start-up outcome had nobody waiting for it");
    }
}

/// Whether the admin endpoint has stopped serving.
static STOPPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Marks the admin endpoint stopped when dropped.
struct Stopped;

impl Drop for Stopped {
    fn drop(&mut self) {
        STOPPED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
}

/// Whether the admin endpoint, once started, has stopped: zainod then runs
/// with no `/metrics` or probes, which the indexer treats as a failure.
pub(crate) fn stopped() -> bool {
    STOPPED.load(std::sync::atomic::Ordering::Relaxed)
}

async fn serve(listener: tokio::net::TcpListener, handle: PrometheusHandle) {
    // The listener was bound before the recorder was installed (`metrics::init`),
    // so every sample recorded from here on has this loop, or a scrape, to drain it
    let upkeep = handle.clone();
    tokio::spawn(zaino_component::supervised(
        zaino_component::CancellationToken::new(),
        async move {
            let mut tick = tokio::time::interval(UPKEEP_INTERVAL);
            loop {
                tick.tick().await;
                upkeep.run_upkeep();
            }
        },
        |reason| {
            error!(%reason, "the metrics upkeep task ended");
            STOPPED.store(true, std::sync::atomic::Ordering::Relaxed);
        },
    ));

    let endpoint = listener.local_addr();
    info!(endpoint = ?endpoint, "admin endpoint started: /metrics, /livez, /readyz");

    let connections = Arc::new(tokio::sync::Semaphore::new(MAX_CONNECTIONS));
    loop {
        // Held for the connection's life (the cap bounds fds and tasks)
        let Ok(permit) = Arc::clone(&connections).acquire_owned().await else {
            return;
        };
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                // Backoff, not `continue`: EMFILE keeps the listener readable → 100% CPU spin
                warn!(%e, "admin connection failed to accept");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let handle = handle.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let service = service_fn(move |request| {
                let handle = handle.clone();
                async move { Ok::<_, Infallible>(route(&handle, &request).await) }
            });
            // `timer` load-bearing: without it hyper drops its header-read timeout (slowloris)
            if let Err(error) = http1::Builder::new()
                .timer(TokioTimer::new())
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                // one client's connection: it alone is dropped
                tracing::debug!(%error, "admin connection ended with an error");
            }
        });
    }
}

async fn route(
    handle: &PrometheusHandle,
    request: &Request<hyper::body::Incoming>,
) -> Response<Full<Bytes>> {
    match request.uri().path() {
        // Off the admin runtime: `render` walks every handle, and this thread answers probes
        "/metrics" => {
            let handle = handle.clone();
            match tokio::task::spawn_blocking(move || {
                crate::metrics::collect_process_metrics();
                handle.render()
            })
            .await
            {
                Ok(scrape) => body(StatusCode::OK, EXPOSITION_CONTENT_TYPE, scrape),
                Err(e) => {
                    error!(%e, "rendering the scrape panicked");
                    body(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        PLAIN_CONTENT_TYPE,
                        String::new(),
                    )
                }
            }
        }
        "/livez" => match is_fresh(last_heartbeat()) {
            true => body(StatusCode::OK, PLAIN_CONTENT_TYPE, "ok".to_string()),
            false => body(
                StatusCode::SERVICE_UNAVAILABLE,
                PLAIN_CONTENT_TYPE,
                "unavailable".to_string(),
            ),
        },
        "/readyz" => readyz(last_readiness()),
        _ => body(StatusCode::NOT_FOUND, PLAIN_CONTENT_TYPE, String::new()),
    }
}

/// `200` while the daemon answers requests, else `503`; the body lists every
/// component and index either way. Not ready until the loop has reported.
fn readyz(readiness: Option<Readiness>) -> Response<Full<Bytes>> {
    let (ready, report) = match readiness {
        Some(readiness) => (readiness.is_ready(), readiness.to_json()),
        None => (
            false,
            serde_json::json!({ "ready": false, "local": false, "components": [], "indexes": [] }),
        ),
    };
    let status = match ready {
        true => StatusCode::OK,
        false => StatusCode::SERVICE_UNAVAILABLE,
    };
    body(status, JSON_CONTENT_TYPE, report.to_string())
}

const JSON_CONTENT_TYPE: &str = "application/json";

/// Prometheus text exposition format
///
/// - Load-bearing: Prometheus >= 3.0 drops a scrape with a blank `Content-Type`
const EXPOSITION_CONTENT_TYPE: &str = "text/plain; version=0.0.4; charset=utf-8";

const PLAIN_CONTENT_TYPE: &str = "text/plain; charset=utf-8";

fn body(status: StatusCode, content_type: &'static str, payload: String) -> Response<Full<Bytes>> {
    Response::builder()
        .status(status)
        .header(hyper::header::CONTENT_TYPE, content_type)
        .body(Full::new(Bytes::from(payload)))
        .unwrap_or_else(|error| {
            // never an empty 200: a response that cannot be built is a failure
            error!(%error, "an admin response could not be built");
            let mut failed = Response::new(Full::new(Bytes::new()));
            *failed.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            failed
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zaino_component::{ComponentName, Health};
    use zaino_primitives::types::Height;

    /// End-to-end over a real socket: bind, accept loop, hyper wiring, routing
    #[tokio::test]
    async fn the_admin_surface_answers_metrics_and_livez() {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        metrics::with_local_recorder(&recorder, || {
            metrics::counter!("zaino.test.total").increment(7);
        });

        let listener = bind(
            "127.0.0.1:0"
                .parse()
                .expect("a loopback socket address parses"),
        )
        .expect("loopback bind succeeds");
        let endpoint = listener
            .local_addr()
            .expect("a bound listener has an address");
        let listener = tokio::net::TcpListener::from_std(listener).expect("the listener registers");
        tokio::spawn(serve(listener, handle));

        // The accept loop races the connect; retry rather than sleep a fixed guess
        let get = |path: &'static str| async move {
            for _ in 0..50 {
                if let Ok(mut stream) = tokio::net::TcpStream::connect(endpoint).await {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let request =
                        format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n");
                    stream.write_all(request.as_bytes()).await.unwrap();
                    let mut response = String::new();
                    stream.read_to_string(&mut response).await.unwrap();
                    return response;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            panic!("admin endpoint never accepted a connection on {endpoint}");
        };

        let metrics = get("/metrics").await;
        assert!(metrics.starts_with("HTTP/1.1 200"), "{metrics}");
        assert!(metrics.contains("zaino_test_total 7"), "{metrics}");
        assert!(
            metrics
                .to_ascii_lowercase()
                .contains("content-type: text/plain; version=0.0.4"),
            "{metrics}"
        );
        assert!(get("/livez").await.starts_with("HTTP/1.1 200"));
        let starting = get("/readyz").await;
        assert!(starting.starts_with("HTTP/1.1 503"), "{starting}");
        assert!(get("/nope").await.starts_with("HTTP/1.1 404"));
    }

    fn status(name: &'static str, lifecycle: Lifecycle, health: Health) -> ComponentStatus {
        ComponentStatus::new(ComponentName(name), lifecycle, health)
    }

    fn readiness(head: Lifecycle, sync: Health) -> Readiness {
        Readiness {
            serving: vec![
                status("chain head", head, Health::Healthy),
                status("gRPC", Lifecycle::Ready, Health::Healthy),
            ],
            building: vec![status("sync", Lifecycle::Syncing, sync)],
            indexes: vec![
                (IndexId::CompactBlock, Serviceability::Absent),
                (
                    IndexId::TreeState,
                    Serviceability::Building(Height::try_from(7).ok()),
                ),
                (
                    IndexId::Spent,
                    Serviceability::ToHeight(Height::try_from(9).expect("in range")),
                ),
            ],
            local: false,
        }
    }

    fn report(response: &Response<Full<Bytes>>) -> StatusCode {
        response.status()
    }

    /// Ready while the serving components are ready, whatever is still
    /// building; not ready while one is not, when the sync has failed, or
    /// before the loop has reported.
    #[test]
    fn readyz_gates_on_serving_not_on_building() {
        let ready = readiness(Lifecycle::Ready, Health::Healthy);
        assert_eq!(report(&readyz(Some(ready.clone()))), StatusCode::OK);
        for unready in [
            readiness(Lifecycle::Syncing, Health::Healthy),
            readiness(Lifecycle::Ready, Health::Critical),
        ] {
            assert_eq!(
                report(&readyz(Some(unready))),
                StatusCode::SERVICE_UNAVAILABLE
            );
        }
        assert_eq!(report(&readyz(None)), StatusCode::SERVICE_UNAVAILABLE);

        let json = ready.to_json();
        assert_eq!(json["ready"], true);
        assert_eq!(json["local"], false);
        assert_eq!(json["components"][0]["name"], "chain head");
        assert_eq!(json["components"][2]["lifecycle"], "Syncing");
        assert_eq!(
            json["indexes"],
            serde_json::json!([
                { "name": IndexId::CompactBlock.name(), "state": "absent", "height": null },
                { "name": IndexId::TreeState.name(), "state": "building", "height": 7 },
                { "name": IndexId::Spent.name(), "state": "built", "height": 9 },
            ])
        );
    }

    /// A port held by another socket fails the bind with an error that names the endpoint.
    #[test]
    fn a_port_in_use_fails_the_bind() {
        let holder = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback bind succeeds");
        let endpoint = holder
            .local_addr()
            .expect("a bound listener has an address");
        let error = bind(endpoint).expect_err("a second bind on a held port fails");
        assert!(error.to_string().contains(&endpoint.to_string()), "{error}");
    }

    /// - Stale heartbeat = wedged indexer runtime
    #[test]
    fn a_stale_heartbeat_is_not_live() {
        let Some(at) = Instant::now().checked_sub(HEARTBEAT_MAX_AGE + Duration::from_secs(1))
        else {
            return; // monotonic clock younger than the max age (fresh boot)
        };
        assert!(!is_fresh(Some(at)));
        assert!(is_fresh(Some(Instant::now())));
        assert!(is_fresh(None), "never beaten = still starting");
    }
}
