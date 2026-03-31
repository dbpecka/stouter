use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::info;

/// Run a minimal REST API on `127.0.0.1:{port}`.
///
/// Endpoints:
///   GET /services         — list all tunneled services and their local ports
///   GET /services/{name}  — look up a single service by name
pub async fn run_api(
    service_ports: Arc<RwLock<HashMap<String, u16>>>,
    port: u16,
) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("bind API socket on 127.0.0.1:{port}"))?;

    info!("REST API listening on 127.0.0.1:{port}");

    loop {
        let (mut stream, _) = listener.accept().await.context("API accept")?;
        let ports = service_ports.clone();

        tokio::spawn(async move {
            let mut buf = vec![0u8; 4096];
            let n = match stream.read(&mut buf).await {
                Ok(n) if n > 0 => n,
                _ => return,
            };

            let request = match std::str::from_utf8(&buf[..n]) {
                Ok(s) => s,
                Err(_) => return,
            };

            let request_line = match request.lines().next() {
                Some(line) => line,
                None => return,
            };

            let parts: Vec<&str> = request_line.split_whitespace().collect();
            if parts.len() < 2 {
                return;
            }

            let method = parts[0];
            let path = parts[1];

            let (status, body) = handle_request(method, path, &ports).await;

            let response = format!(
                "HTTP/1.1 {status}\r\n\
                 Content-Type: application/json\r\n\
                 Content-Length: {}\r\n\
                 Connection: close\r\n\
                 \r\n\
                 {body}",
                body.len()
            );

            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}

async fn handle_request(
    method: &str,
    path: &str,
    service_ports: &Arc<RwLock<HashMap<String, u16>>>,
) -> (&'static str, String) {
    if method != "GET" {
        return (
            "405 Method Not Allowed",
            r#"{"error":"method not allowed"}"#.to_string(),
        );
    }

    let path = path.trim_end_matches('/');

    if path == "/services" {
        let ports = service_ports.read().await;
        let services: Vec<serde_json::Value> = ports
            .iter()
            .map(|(name, port)| {
                serde_json::json!({
                    "name": name,
                    "port": port,
                    "address": format!("127.0.0.1:{port}"),
                })
            })
            .collect();
        ("200 OK", serde_json::to_string(&services).unwrap())
    } else if let Some(name) = path.strip_prefix("/services/") {
        if name.is_empty() {
            return (
                "400 Bad Request",
                r#"{"error":"missing service name"}"#.to_string(),
            );
        }
        let ports = service_ports.read().await;
        match ports.get(name) {
            Some(&port) => (
                "200 OK",
                serde_json::to_string(&serde_json::json!({
                    "name": name,
                    "port": port,
                    "address": format!("127.0.0.1:{port}"),
                }))
                .unwrap(),
            ),
            None => (
                "404 Not Found",
                serde_json::json!({"error": format!("service '{name}' not found")}).to_string(),
            ),
        }
    } else {
        (
            "404 Not Found",
            r#"{"error":"not found"}"#.to_string(),
        )
    }
}
