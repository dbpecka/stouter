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

type AppState = Arc<RwLock<HashMap<String, ServiceInfo>>>;

/// Run a minimal REST API on `127.0.0.1:{port}`.
///
/// Endpoints:
///   GET /services         — list all tunneled services and their local ports
///   GET /services/{name}  — look up a single service by name
pub async fn run_api(service_ports: AppState, port: u16) -> Result<()> {
    let app = Router::new()
        .route("/services", get(list_services))
        .route("/services/{name}", get(get_service))
        .with_state(service_ports);

    let listener = TcpListener::bind(format!("127.0.0.1:{port}"))
        .await
        .with_context(|| format!("bind API socket on 127.0.0.1:{port}"))?;

    info!("REST API listening on 127.0.0.1:{port}");

    axum::serve(listener, app)
        .await
        .context("REST API server error")?;

    Ok(())
}

async fn list_services(State(ports): State<AppState>) -> Json<Vec<serde_json::Value>> {
    let ports = ports.read().await;
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
    State(ports): State<AppState>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    if name.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "missing service name"})),
        );
    }

    let ports = ports.read().await;
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
        let state: AppState = Arc::new(RwLock::new(map));
        Router::new()
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
}
