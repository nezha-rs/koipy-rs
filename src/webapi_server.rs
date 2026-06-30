use anyhow::{Context, Result, bail};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, Method};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;

use crate::config::KoipyConfig;
use crate::progress::ProgressReport;

#[derive(Debug)]
struct WebApiState {
    config: RwLock<KoipyConfig>,
}

pub async fn serve_webapi(config: KoipyConfig) -> Result<()> {
    if !config.webapi.enable {
        return Ok(());
    }
    let addr: SocketAddr = config
        .webapi
        .address
        .parse()
        .with_context(|| format!("invalid webapi.address {}", config.webapi.address))?;
    tracing::info!(%addr, "Web API started");
    let app = router(config.clone());
    if config.webapi.tls {
        let tls = load_tls_config(&config).await?;
        axum_server::bind_rustls(addr, tls)
            .serve(app.into_make_service())
            .await?;
    } else {
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind webapi {}", addr))?;
        axum::serve(listener, app).await?;
    }
    Ok(())
}

pub fn router(config: KoipyConfig) -> Router {
    let cors = cors_layer(&config);
    Router::new()
        .route("/health", get(health))
        .route("/progress", get(progress))
        .route("/config/summary", get(config_summary))
        .route("/config/users/grant", post(grant_user))
        .with_state(Arc::new(WebApiState {
            config: RwLock::new(config),
        }))
        .layer(cors)
}

async fn health() -> impl IntoResponse {
    Json(json!({ "ok": true, "service": "koipy-rs" }))
}

async fn progress(
    State(state): State<Arc<WebApiState>>,
    headers: HeaderMap,
    query: Query<AuthQuery>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    if let Err(err) = authorize(&config, &headers, query.password.as_deref()) {
        return (axum::http::StatusCode::UNAUTHORIZED, err.to_string()).into_response();
    }
    Json(json!({
        "overall": ProgressReport::current().overall(),
        "markdown": ProgressReport::current().render_markdown(),
    }))
    .into_response()
}

async fn config_summary(
    State(state): State<Arc<WebApiState>>,
    headers: HeaderMap,
    query: Query<AuthQuery>,
) -> impl IntoResponse {
    let config = state.config.read().await;
    if let Err(err) = authorize(&config, &headers, query.password.as_deref()) {
        return (axum::http::StatusCode::UNAUTHORIZED, err.to_string()).into_response();
    }
    Json(json!({
        "summary": config.summary(),
        "slaves": config.visible_slaves().len(),
        "scripts": config.script_config.scripts.len(),
        "webapi": {
            "enabled": config.webapi.enable,
            "address": config.webapi.address,
            "tls": config.webapi.tls,
        }
    }))
    .into_response()
}

async fn grant_user(
    State(state): State<Arc<WebApiState>>,
    headers: HeaderMap,
    query: Query<AuthQuery>,
    Json(payload): Json<GrantUserRequest>,
) -> impl IntoResponse {
    let mut config = state.config.write().await;
    if let Err(err) = authorize(&config, &headers, query.password.as_deref()) {
        return (axum::http::StatusCode::UNAUTHORIZED, err.to_string()).into_response();
    }
    let changed = config.grant_user(payload.user_id);
    if changed {
        if let Err(err) = config.save_to_source() {
            return (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                err.to_string(),
            )
                .into_response();
        }
    }
    Json(GrantUserResponse {
        user_id: payload.user_id,
        changed,
        users: config.user.len(),
    })
    .into_response()
}

#[derive(Debug, Deserialize)]
struct AuthQuery {
    password: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GrantUserRequest {
    user_id: i64,
}

#[derive(Debug, Serialize)]
struct GrantUserResponse {
    user_id: i64,
    changed: bool,
    users: usize,
}

fn authorize(
    config: &KoipyConfig,
    headers: &HeaderMap,
    query_password: Option<&str>,
) -> Result<()> {
    let password = config.webapi.password.trim();
    if password.is_empty() {
        return Ok(());
    }
    if query_password == Some(password) {
        return Ok(());
    }
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if bearer == Some(password) {
        return Ok(());
    }
    bail!("webapi authorization failed")
}

fn cors_layer(config: &KoipyConfig) -> CorsLayer {
    let layer = CorsLayer::new().allow_methods([Method::GET]);
    if config
        .webapi
        .allow_origins
        .iter()
        .any(|origin| origin == "*")
    {
        return layer.allow_origin(tower_http::cors::Any);
    }
    let origins: Vec<HeaderValue> = config
        .webapi
        .allow_origins
        .iter()
        .filter_map(|origin| origin.parse().ok())
        .collect();
    if origins.is_empty() {
        layer
    } else {
        layer.allow_origin(origins)
    }
}

async fn load_tls_config(config: &KoipyConfig) -> Result<RustlsConfig> {
    if config.webapi.cert_path.trim().is_empty() || config.webapi.key_path.trim().is_empty() {
        bail!("webapi.tls requires webapi.certPath and webapi.keyPath");
    }
    RustlsConfig::from_pem_file(&config.webapi.cert_path, &config.webapi.key_path)
        .await
        .context("failed to load webapi TLS certificate/key")
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    #[tokio::test]
    async fn health_is_public() {
        let app = router(KoipyConfig::default());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn progress_requires_password_when_configured() {
        let mut config = KoipyConfig::default();
        config.webapi.password = "secret".to_string();
        let app = router(config);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/progress")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("unauthorized");
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/progress")
                    .header(axum::http::header::AUTHORIZATION, "Bearer secret")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("authorized");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
    }

    #[tokio::test]
    async fn grant_user_persists_to_config_source() {
        let path =
            std::env::temp_dir().join(format!("koipy-rs-webapi-grant-{}.yaml", std::process::id()));
        std::fs::write(
            &path,
            r#"
webapi:
  password: secret
user: []
"#,
        )
        .expect("seed");
        let mut config = KoipyConfig::from_path(&path).expect("config");
        config.webapi.password = "secret".to_string();
        let app = router(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/config/users/grant")
                    .header(axum::http::header::AUTHORIZATION, "Bearer secret")
                    .header(axum::http::header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"user_id":67890}"#))
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let reloaded = KoipyConfig::from_path(&path).expect("reload");
        assert!(
            reloaded
                .user
                .iter()
                .any(|value| matches!(value, serde_yaml::Value::Number(number) if number.as_i64() == Some(67890)))
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn tls_requires_cert_and_key_paths() {
        let mut config = KoipyConfig::default();
        config.webapi.tls = true;
        let err = load_tls_config(&config)
            .await
            .expect_err("missing cert/key should fail");
        assert!(err.to_string().contains("certPath"));
    }
}
