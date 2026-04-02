use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::info;

use super::ServiceInfo;

/// Run a minimal REST API on `127.0.0.1:{port}`.
///
/// Endpoints:
///   GET /services         — list all tunneled services and their local ports
///   GET /services/{name}  — look up a single service by name
pub async fn run_api(
    service_ports: Arc<RwLock<HashMap<String, ServiceInfo>>>,
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
    service_ports: &Arc<RwLock<HashMap<String, ServiceInfo>>>,
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
            .map(|(name, info)| {
                serde_json::json!({
                    "name": name,
                    "port": info.port,
                    "address": format!("127.0.0.1:{}", info.port),
                    "domains": info.domains,
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
            Some(info) => (
                "200 OK",
                serde_json::to_string(&serde_json::json!({
                    "name": name,
                    "port": info.port,
                    "address": format!("127.0.0.1:{}", info.port),
                    "domains": info.domains,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ports(entries: &[(&str, u16, &[&str])]) -> Arc<RwLock<HashMap<String, ServiceInfo>>> {
        let map: HashMap<String, ServiceInfo> = entries
            .iter()
            .map(|(k, v, domains)| {
                (
                    k.to_string(),
                    ServiceInfo {
                        port: *v,
                        domains: domains.iter().map(|d| d.to_string()).collect(),
                    },
                )
            })
            .collect();
        Arc::new(RwLock::new(map))
    }

    #[tokio::test]
    async fn get_services_empty() {
        let ports = make_ports(&[]);
        let (status, body) = handle_request("GET", "/services", &ports).await;
        assert_eq!(status, "200 OK");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert!(arr.is_empty());
    }

    #[tokio::test]
    async fn get_services_with_entries() {
        let ports = make_ports(&[("web", 8080, &[]), ("db", 5432, &[])]);
        let (status, body) = handle_request("GET", "/services", &ports).await;
        assert_eq!(status, "200 OK");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(arr.len(), 2);
    }

    #[tokio::test]
    async fn get_single_service_found() {
        let ports = make_ports(&[("web", 8080, &[])]);
        let (status, body) = handle_request("GET", "/services/web", &ports).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["name"], "web");
        assert_eq!(v["port"], 8080);
        assert_eq!(v["address"], "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn get_single_service_not_found() {
        let ports = make_ports(&[]);
        let (status, body) = handle_request("GET", "/services/missing", &ports).await;
        assert_eq!(status, "404 Not Found");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("missing"));
    }

    #[tokio::test]
    async fn post_returns_405() {
        let ports = make_ports(&[]);
        let (status, _) = handle_request("POST", "/services", &ports).await;
        assert_eq!(status, "405 Method Not Allowed");
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let ports = make_ports(&[]);
        let (status, _) = handle_request("GET", "/unknown", &ports).await;
        assert_eq!(status, "404 Not Found");
    }

    #[tokio::test]
    async fn trailing_slash_normalized() {
        let ports = make_ports(&[("web", 8080, &[])]);
        let (status, body) = handle_request("GET", "/services/", &ports).await;
        // "/services/" with trailing slash stripped becomes "/services"
        assert_eq!(status, "200 OK");
        let arr: Vec<serde_json::Value> = serde_json::from_str(&body).unwrap();
        assert_eq!(arr.len(), 1);
    }

    #[tokio::test]
    async fn get_service_with_domains() {
        let ports = make_ports(&[("web", 8080, &["example.com", "www.example.com"])]);
        let (status, body) = handle_request("GET", "/services/web", &ports).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["name"], "web");
        let domains = v["domains"].as_array().unwrap();
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0], "example.com");
        assert_eq!(domains[1], "www.example.com");
    }

    #[tokio::test]
    async fn get_service_without_domains() {
        let ports = make_ports(&[("web", 8080, &[])]);
        let (status, body) = handle_request("GET", "/services/web", &ports).await;
        assert_eq!(status, "200 OK");
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let domains = v["domains"].as_array().unwrap();
        assert!(domains.is_empty());
    }
}
