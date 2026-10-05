//! Minimal HTTP health endpoint for Kubernetes probes.
//!
//! The bot has no server of its own (it's a Telegram long-poller plus JMAP
//! EventSource watchers), so the chart previously shipped without any
//! probe. This module adds the smallest possible surface that answers
//! kubelet: a hand-rolled HTTP/1.1 responder on a dedicated listener —
//! no HTTP framework dependency, no shared state to deadlock.
//!
//! Semantics, deliberately minimal (v1): both `/healthz` and `/readyz`
//! answer 200 as long as the tokio runtime is scheduling this task and
//! the listener accepts connections. If the runtime stalls or the
//! process wedges hard, the TCP accept loop stops keeping up and the
//! kubelet's timeout kills the pod. Per-watcher liveness (e.g. "the JMAP
//! EventSource for chat X has been silent for N minutes") is a richer
//! signal that belongs in the watcher loop, not in the probe contract —
//! a false-positive liveness kill is worse for a single-replica stateful
//! bot than a hung watcher, which reconnects on its own backoff loop.

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Binds the probe listener. Called from `main` rather than inside the
/// spawned task so a bind failure (port already taken, misconfigured
/// environment) is a startup error, not a silently missing probe.
pub async fn bind() -> std::io::Result<TcpListener> {
    let listener = TcpListener::bind("0.0.0.0:8080").await?;
    tracing::info!(addr = %listener.local_addr()?, "health endpoint listening");
    Ok(listener)
}

/// Serves probe requests until the process exits. Never returns on its
/// own; run it in a spawned task.
pub async fn serve(listener: TcpListener) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(async move {
                    if let Err(e) = respond(stream).await {
                        // A client hanging up mid-request lands here; not
                        // worth more than a trace line.
                        tracing::trace!(error = %e, "health connection error");
                    }
                });
            }
            Err(e) => {
                // accept() errors are transient by definition (ECONNABORTED
                // and friends); log and keep serving. A truly broken
                // listener surfaces as probes timing out instead.
                tracing::warn!(error = %e, "health accept error");
            }
        }
    }
}

/// Reads one HTTP request head and answers it. Both endpoints share the
/// same 200: see the module docs for why there is no richer signal yet.
async fn respond(mut stream: TcpStream) -> std::io::Result<()> {
    // Probes send a fixed, tiny request line + headers; one read of a
    // small buffer is enough (kubelet never sends bodies here). Reading
    // exactly once and replying immediately keeps this allocation-free.
    let mut buf = [0u8; 512];
    let n = stream.read(&mut buf).await?;
    let request = String::from_utf8_lossy(&buf[..n]);
    let path = request.split_whitespace().nth(1).unwrap_or_default();

    let (status, body) = match path {
        "/healthz" | "/readyz" => ("200 OK", "ok\n"),
        _ => ("404 Not Found", ""),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    async fn spawn_ephemeral() -> std::net::SocketAddr {
        // Port 0 lets the OS pick a free port, so tests never race each
        // other or a concurrently running dev instance on 8080.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(listener));
        addr
    }

    async fn get(addr: std::net::SocketAddr, path: &str) -> (u16, String) {
        let mut stream = TcpStream::connect(addr).await.unwrap();
        stream
            .write_all(format!("GET {path} HTTP/1.1\r\nHost: probe\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = Vec::new();
        tokio::time::timeout(Duration::from_secs(2), stream.read_to_end(&mut buf))
            .await
            .expect("response within timeout")
            .unwrap();
        let text = String::from_utf8_lossy(&buf).into_owned();
        let status: u16 = text
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let body = text
            .split("\r\n\r\n")
            .nth(1)
            .unwrap_or_default()
            .to_string();
        (status, body)
    }

    #[tokio::test]
    async fn healthz_answers_200_ok() {
        let addr = spawn_ephemeral().await;
        let (status, body) = get(addr, "/healthz").await;
        assert_eq!(status, 200);
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn readyz_answers_200_ok() {
        let addr = spawn_ephemeral().await;
        let (status, body) = get(addr, "/readyz").await;
        assert_eq!(status, 200);
        assert_eq!(body, "ok\n");
    }

    #[tokio::test]
    async fn unknown_path_is_404_with_empty_body() {
        let addr = spawn_ephemeral().await;
        let (status, body) = get(addr, "/nope").await;
        assert_eq!(status, 404);
        assert_eq!(body, "");
    }

    #[tokio::test]
    async fn serves_many_consecutive_probes() {
        // The kubelet opens a fresh connection per probe: make sure accept
        // + spawn keeps working across several requests.
        let addr = spawn_ephemeral().await;
        for _ in 0..20 {
            assert_eq!(get(addr, "/healthz").await.0, 200);
        }
    }
}
