//! Shared process-lifecycle helpers for Scrapix services on Fly.io.
//!
//! Three primitives, composed per service:
//! - [`install_signal_handlers`] — flips a shutdown flag on `SIGTERM` or Ctrl-C.
//!   Fly sends SIGTERM before SIGKILL; without a handler, in-flight Kafka
//!   messages lose their offset commit and get reprocessed.
//! - [`spawn_wake_listener`] — tiny HTTP responder on a configurable port.
//!   Fly's proxy uses an incoming TCP connection on a declared port to
//!   auto-start a suspended machine, so accepting the connection at all is
//!   what matters for that; on top of it, it serves `GET /metrics`
//!   (Prometheus text format, from the shared [`scrapix_core::metrics`]
//!   registry) and `GET /health` (`ok`) unauthenticated. Any other request,
//!   or a non-HTTP connection, still gets accepted (and so still counts as
//!   a wake) and is closed with a generic `200 OK`.
//! - [`spawn_idle_watchdog`] — watches a monotonically-increasing counter
//!   (typically `messages_processed`); flips the shutdown flag if it doesn't
//!   advance for `idle_minutes`. Lets Kafka-consumer workers exit cleanly
//!   when idle so Fly can suspend them to zero cost.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::signal::unix::{signal, SignalKind};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

/// Generic response for anything that isn't a recognized `GET /metrics` or
/// `GET /health` request: a non-HTTP connection, a HEAD/POST, an unknown
/// path, or a request line we failed to read within the timeout. Fly's
/// autostart only cares that the connection was accepted, so this is a
/// harmless catch-all rather than an error.
const GENERIC_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n";

const HEALTH_RESPONSE: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n";

/// How long to wait for a request line before giving up and sending the
/// generic response. Keeps a slow/garbage client from tying up a task
/// forever; legitimate `curl`/Fly-proxy requests arrive well within this.
const REQUEST_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Install SIGTERM + Ctrl-C handlers that flip `shutdown` to `true` on the
/// first signal. Returns a `JoinHandle` you can drop; the task exits on its
/// own once a signal fires.
pub fn install_signal_handlers(shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "Failed to register SIGTERM handler");
                return;
            }
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("Received SIGINT (Ctrl-C), initiating graceful shutdown");
            }
            _ = term.recv() => {
                info!("Received SIGTERM, initiating graceful shutdown");
            }
        }
        shutdown.store(true, Ordering::Relaxed);
    })
}

/// Spawn a bare-bones TCP listener that accepts connections on `port` and
/// responds `200 OK` to any payload. Exists solely so Fly.io's proxy sees
/// the port open and will autostart the machine on incoming connections
/// (e.g. from the API's worker-wake fan-out).
///
/// Exits when `shutdown` flips to `true`.
pub fn spawn_wake_listener(port: u16, shutdown: Arc<AtomicBool>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let bind_addr = format!("0.0.0.0:{}", port);
        let listener = match TcpListener::bind(&bind_addr).await {
            Ok(l) => l,
            Err(e) => {
                warn!(port, error = %e, "Wake listener failed to bind; Fly autostart will not work for this machine");
                return;
            }
        };
        info!(port, "Wake listener ready");
        serve_wake_listener(listener, shutdown).await;
    })
}

/// Accept loop shared by [`spawn_wake_listener`] and its tests: the tests
/// bind their own ephemeral-port listener (so they can learn the real
/// address) and drive this directly instead of going through the
/// port-from-env bind above.
async fn serve_wake_listener(listener: TcpListener, shutdown: Arc<AtomicBool>) {
    loop {
        if shutdown.load(Ordering::Relaxed) {
            debug!("Wake listener shutting down");
            return;
        }
        let accept = tokio::time::timeout(Duration::from_secs(1), listener.accept()).await;
        let (stream, _) = match accept {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                debug!(error = %e, "Wake listener accept error");
                continue;
            }
            Err(_) => continue, // timeout — loop and recheck shutdown
        };
        tokio::spawn(handle_wake_connection(stream));
    }
}

/// Handle one accepted connection: hand-parse just the request line to
/// decide between `/metrics`, `/health`, and everything else. Never panics —
/// a connection that sends garbage, nothing, or closes immediately still
/// gets a response attempt (best-effort) and the task exits cleanly either
/// way, so the wake it triggered by merely connecting is never lost.
async fn handle_wake_connection(mut stream: TcpStream) {
    let mut buf = [0u8; 2048];
    let read = tokio::time::timeout(REQUEST_READ_TIMEOUT, stream.read(&mut buf)).await;
    let n = match read {
        Ok(Ok(n)) => n,
        _ => 0, // timeout, error, or EOF with nothing sent: fall through to generic response
    };
    let request = String::from_utf8_lossy(&buf[..n]);
    let request_line = request.lines().next().unwrap_or("");

    let response: Vec<u8> = if is_get(request_line, "/metrics") {
        let body = scrapix_core::metrics::encode();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            scrapix_core::metrics::CONTENT_TYPE,
            body.len(),
            body
        )
        .into_bytes()
    } else if is_get(request_line, "/health") {
        HEALTH_RESPONSE.to_vec()
    } else {
        GENERIC_RESPONSE.to_vec()
    };

    let _ = stream.write_all(&response).await;
    let _ = stream.shutdown().await;
}

/// Whether `request_line` (e.g. `"GET /metrics HTTP/1.1"`) is a `GET` for
/// exactly `path`, ignoring any query string.
fn is_get(request_line: &str, path: &str) -> bool {
    let mut parts = request_line.split_whitespace();
    let Some(method) = parts.next() else {
        return false;
    };
    let Some(target) = parts.next() else {
        return false;
    };
    let target_path = target.split('?').next().unwrap_or(target);
    method.eq_ignore_ascii_case("GET") && target_path == path
}

/// Watch a monotonically-increasing counter (typically messages processed)
/// and flip `shutdown` to `true` once it fails to advance for `idle_minutes`
/// of wall-clock time. `idle_minutes <= 0.0` disables the watchdog.
///
/// `sample_counter` is invoked periodically; it should return the current
/// cumulative count (e.g. `metrics.urls_processed.load(Ordering::Relaxed)`).
pub fn spawn_idle_watchdog<F>(
    sample_counter: F,
    idle_minutes: f64,
    shutdown: Arc<AtomicBool>,
) -> JoinHandle<()>
where
    F: Fn() -> u64 + Send + Sync + 'static,
{
    tokio::spawn(async move {
        if idle_minutes <= 0.0 {
            debug!("Idle watchdog disabled (idle_minutes <= 0)");
            return;
        }
        let idle_duration = Duration::from_secs_f64(idle_minutes * 60.0);
        let sample_interval = Duration::from_secs_f64((idle_minutes * 60.0 / 6.0).max(10.0));

        let mut last_count = sample_counter();
        let mut last_change = tokio::time::Instant::now();
        info!(
            idle_minutes,
            sample_secs = sample_interval.as_secs(),
            "Idle watchdog started"
        );

        loop {
            tokio::time::sleep(sample_interval).await;
            if shutdown.load(Ordering::Relaxed) {
                return;
            }
            let current = sample_counter();
            if current != last_count {
                last_count = current;
                last_change = tokio::time::Instant::now();
                continue;
            }
            if last_change.elapsed() >= idle_duration {
                info!(
                    idle_minutes,
                    processed = current,
                    "Idle watchdog: no work for configured window, triggering shutdown"
                );
                shutdown.store(true, Ordering::Relaxed);
                return;
            }
        }
    })
}

/// Convenience: read `IDLE_EXIT_MINUTES` env var, defaulting to `default_minutes`.
/// Set to `0` to disable. Invalid values fall back to the default with a warning.
pub fn idle_minutes_from_env(default_minutes: f64) -> f64 {
    match std::env::var("IDLE_EXIT_MINUTES") {
        Ok(s) => s.parse::<f64>().unwrap_or_else(|_| {
            warn!(
                value = %s,
                "IDLE_EXIT_MINUTES is not a valid number, using default"
            );
            default_minutes
        }),
        Err(_) => default_minutes,
    }
}

/// Convenience: read `WAKE_PORT` env var, defaulting to `8081`.
pub fn wake_port_from_env() -> u16 {
    std::env::var("WAKE_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8081)
}

#[cfg(test)]
mod wake_listener_tests {
    use super::*;

    /// Bind an ephemeral-port listener, serve it in the background, and
    /// return its address plus the shutdown flag to stop it with.
    async fn spawn_test_server() -> (std::net::SocketAddr, Arc<AtomicBool>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = Arc::new(AtomicBool::new(false));
        tokio::spawn(serve_wake_listener(listener, shutdown.clone()));
        (addr, shutdown)
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_text_with_a_registered_counter() {
        // Touch a counter so `encode()` has deterministic content to assert on.
        scrapix_core::metrics::crawler_bytes_total().inc_by(0.0);

        let (addr, shutdown) = spawn_test_server().await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /metrics HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        stream.shutdown().await.ok();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);

        assert!(
            response.contains("Content-Type: text/plain; version=0.0.4"),
            "response: {response}"
        );
        assert!(
            response.contains("scrapix_crawler_bytes_total"),
            "response must contain a registered counter: {response}"
        );

        shutdown.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn health_endpoint_returns_ok() {
        let (addr, shutdown) = spawn_test_server().await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: test\r\n\r\n")
            .await
            .unwrap();
        stream.shutdown().await.ok();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);

        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "response: {response}"
        );
        assert!(response.ends_with("ok\n"), "response: {response}");

        shutdown.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn raw_non_http_connection_still_gets_accepted_and_does_not_panic() {
        let (addr, shutdown) = spawn_test_server().await;
        let mut stream = TcpStream::connect(addr).await.unwrap();
        // Garbage, not an HTTP request line at all.
        stream
            .write_all(b"\x00\x01\x02not http\xff\xff")
            .await
            .unwrap();
        stream.shutdown().await.ok();

        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response);
        // Still gets a well-formed (generic) HTTP response, and the server
        // task did not panic handling it.
        assert!(
            response.starts_with("HTTP/1.1 200 OK"),
            "response: {response}"
        );

        shutdown.store(true, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn connection_with_no_data_still_counts_as_a_wake() {
        // Fly's proxy just needs the TCP connection accepted; a client that
        // connects and disconnects without sending anything must not hang
        // the server or panic it.
        let (addr, shutdown) = spawn_test_server().await;
        let stream = TcpStream::connect(addr).await.unwrap();
        drop(stream); // immediate close, no bytes sent

        // The server should still be alive and answering other connections.
        let mut stream2 = TcpStream::connect(addr).await.unwrap();
        stream2
            .write_all(b"GET /health HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        stream2.shutdown().await.ok();
        let mut response = Vec::new();
        stream2.read_to_end(&mut response).await.unwrap();
        assert!(String::from_utf8_lossy(&response).ends_with("ok\n"));

        shutdown.store(true, Ordering::Relaxed);
    }

    #[test]
    fn is_get_matches_path_ignoring_query_string() {
        assert!(is_get("GET /metrics HTTP/1.1", "/metrics"));
        assert!(is_get("GET /health?foo=bar HTTP/1.1", "/health"));
        assert!(!is_get("POST /metrics HTTP/1.1", "/metrics"));
        assert!(!is_get("GET /other HTTP/1.1", "/metrics"));
        assert!(!is_get("", "/metrics"));
    }
}
