use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tracing::info;

use super::ServiceInfo;
use crate::state::SharedState;

type ServicePorts = Arc<RwLock<HashMap<String, ServiceInfo>>>;

#[derive(Clone)]
struct AppState {
    service_ports: ServicePorts,
    shared: Arc<SharedState>,
}

/// Run a minimal REST API on `127.0.0.1:{port}`.
///
/// Endpoints:
///   GET /__health          — health check reporting gossip and proxy status
///   GET /services          — list all tunneled services and their local ports
///   GET /services/{name}   — look up a single service by name
pub async fn run_api(
    service_ports: ServicePorts,
    shared: Arc<SharedState>,
    port: u16,
) -> Result<()> {
    let state = AppState {
        service_ports,
        shared,
    };

    let app = Router::new()
        .route("/__health", get(health_check))
        .route("/services", get(list_services))
        .route("/services/{name}", get(get_service))
        .with_state(state);

    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("bind API socket on 127.0.0.1:{port}"))?;

    info!("REST API listening on 127.0.0.1:{port}");

    axum::serve(listener, app)
        .await
        .context("REST API server error")?;

    Ok(())
}

async fn health_check(State(state): State<AppState>) -> impl IntoResponse {
    let nodes = state.shared.known_nodes.load();
    let node_count = nodes.len();

    let connected_nodes: Vec<serde_json::Value> = nodes
        .values()
        .map(|node| {
            let connect_addr = node.relay.as_deref().unwrap_or(&node.addr);
            serde_json::json!({
                "id": node.id,
                "addr": node.addr,
                "relay": node.relay,
                "tunnel_pool": state.shared.tunnel_pool.count(connect_addr),
                "reverse_pool": state.shared.reverse_pool.count(&node.id),
            })
        })
        .collect();

    let services = state.service_ports.read().await;
    let proxy_count = services.len();

    let proxied_services: Vec<serde_json::Value> = services
        .iter()
        .map(|(name, info)| {
            serde_json::json!({
                "name": name,
                "port": info.port,
                "address": format!("127.0.0.1:{}", info.port),
            })
        })
        .collect();

    let healthy = node_count > 0;
    let status = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };

    (
        status,
        Json(serde_json::json!({
            "status": if healthy { "healthy" } else { "degraded" },
            "node_id": state.shared.config.node_id,
            "nodes": {
                "count": node_count,
                "peers": connected_nodes,
            },
            "proxies": {
                "count": proxy_count,
                "services": proxied_services,
            },
        })),
    )
}

async fn list_services(State(state): State<AppState>) -> Json<Vec<serde_json::Value>> {
    let ports = state.service_ports.read().await;
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
    Json(services)
}

async fn get_service(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing service name"})),
        );
    }

    let ports = state.service_ports.read().await;
    match ports.get(&name) {
        Some(info) => (
            StatusCode::OK,
            Json(serde_json::json!({
                "name": name,
                "port": info.port,
                "address": format!("127.0.0.1:{}", info.port),
                "domains": info.domains,
            })),
        ),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"error": format!("service '{name}' not found")})),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use crate::config::Config;
    use tokio_util::sync::CancellationToken;

    fn make_app(entries: &[(&str, u16, &[&str])]) -> Router {
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
        let service_ports: ServicePorts = Arc::new(RwLock::new(map));
        let shared = SharedState::new(Config::default(), String::new(), CancellationToken::new());
        let state = AppState {
            service_ports,
            shared,
        };
        Router::new()
            .route("/__health", get(health_check))
            .route("/services", get(list_services))
            .route("/services/{name}", get(get_service))
            .with_state(state)
    }

    async fn do_request(app: Router, method: &str, uri: &str) -> (StatusCode, serde_json::Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        (status, json)
    }

    #[tokio::test]
    async fn get_services_empty() {
        let (status, body) = do_request(make_app(&[]), "GET", "/services").await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn get_services_with_entries() {
        let (status, body) =
            do_request(make_app(&[("web", 8080, &[]), ("db", 5432, &[])]), "GET", "/services")
                .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body.as_array().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn get_single_service_found() {
        let (status, body) =
            do_request(make_app(&[("web", 8080, &[])]), "GET", "/services/web").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "web");
        assert_eq!(body["port"], 8080);
        assert_eq!(body["address"], "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn get_single_service_not_found() {
        let (status, body) =
            do_request(make_app(&[]), "GET", "/services/missing").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body["error"].as_str().unwrap().contains("missing"));
    }

    #[tokio::test]
    async fn post_returns_405() {
        let (status, _) = do_request(make_app(&[]), "POST", "/services").await;
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn unknown_path_returns_404() {
        let (status, _) = do_request(make_app(&[]), "GET", "/unknown").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn get_service_with_domains() {
        let (status, body) = do_request(
            make_app(&[("web", 8080, &["example.com", "www.example.com"])]),
            "GET",
            "/services/web",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "web");
        let domains = body["domains"].as_array().unwrap();
        assert_eq!(domains.len(), 2);
        assert_eq!(domains[0], "example.com");
        assert_eq!(domains[1], "www.example.com");
    }

    #[tokio::test]
    async fn get_service_without_domains() {
        let (status, body) =
            do_request(make_app(&[("web", 8080, &[])]), "GET", "/services/web").await;
        assert_eq!(status, StatusCode::OK);
        let domains = body["domains"].as_array().unwrap();
        assert!(domains.is_empty());
    }

    #[tokio::test]
    async fn health_check_degraded_no_nodes() {
        let (status, body) = do_request(make_app(&[]), "GET", "/__health").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["status"], "degraded");
        assert_eq!(body["nodes"]["count"], 0);
        assert_eq!(body["proxies"]["count"], 0);
    }

    #[tokio::test]
    async fn health_check_reports_proxies() {
        let (status, body) =
            do_request(make_app(&[("web", 8080, &[])]), "GET", "/__health").await;
        // Still degraded because no gossip nodes, but proxies are reported
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["proxies"]["count"], 1);
        assert_eq!(body["proxies"]["services"][0]["name"], "web");
    }

    #[tokio::test]
    async fn health_check_healthy_with_nodes() {
        use crate::config::NodeInfo;

        let service_ports: ServicePorts = Arc::new(RwLock::new(HashMap::new()));
        let shared = SharedState::new(Config::default(), String::new(), CancellationToken::new());
        shared
            .update_known_nodes(|nodes| {
                nodes.insert(
                    "peer1".into(),
                    NodeInfo {
                        id: "peer1".into(),
                        addr: "10.0.0.1:8080".into(),
                        relay: None,
                    },
                );
            })
            .await;
        let state = AppState {
            service_ports,
            shared,
        };
        let app = Router::new()
            .route("/__health", get(health_check))
            .with_state(state);

        let (status, body) = do_request(app, "GET", "/__health").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "healthy");
        assert_eq!(body["nodes"]["count"], 1);
        assert_eq!(body["nodes"]["peers"][0]["id"], "peer1");
    }
}
