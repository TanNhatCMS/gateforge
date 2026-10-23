// GateForge — self-hosted AI gateway (Rust port of 9router).
// Phase 0: /health, /v1/models (from the extracted registry), and a
// /v1/chat/completions passthrough to a configured OpenAI-compatible upstream.
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;

fn upstream_base() -> String {
    std::env::var("UPSTREAM_BASE_URL").unwrap_or_else(|_| "http://localhost:20228".into())
}

fn upstream_key() -> String {
    std::env::var("UPSTREAM_API_KEY").unwrap_or_default()
}

async fn health() -> Json<Value> {
    Json(serde_json::json!({
        "ok": true,
        "app": "gateforge",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn models() -> Json<Value> {
    let registry = gf_providers::Registry::load_embedded();
    let data: Vec<Value> = registry
        .model_entries()
        .into_iter()
        .map(|(provider, model)| {
            serde_json::json!({
                "id": format!("{provider}/{}", model.id),
                "object": "model",
                "owned_by": provider,
            })
        })
        .collect();
    Json(serde_json::json!({ "object": "list", "data": data }))
}

// Phase 0 passthrough: forward the request body verbatim. The model id keeps
// its "provider/model" prefix — 9router routes on it. Plain OpenAI upstreams
// get the bare id later, behind an explicit opt-in flag.
async fn chat_completions(Json(body): Json<Value>) -> impl IntoResponse {
    let base = upstream_base();
    let key = upstream_key();
    let client = reqwest::Client::new();

    let mut request = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&body);
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            match response.text().await {
                Ok(text) => {
                    let code = StatusCode::from_u16(status.as_u16())
                        .unwrap_or(StatusCode::BAD_GATEWAY);
                    (code, text).into_response()
                }
                Err(error) => (
                    StatusCode::BAD_GATEWAY,
                    format!("upstream body read failed: {error}"),
                )
                    .into_response(),
            }
        }
        Err(error) => (
            StatusCode::BAD_GATEWAY,
            format!("upstream unreachable: {error}"),
        )
            .into_response(),
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gf=info,tower_http=info".into()),
        )
        .init();

    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(20228);

    let registry = gf_providers::Registry::load_embedded();
    tracing::info!(
        "gateforge {} starting with {} providers",
        env!("CARGO_PKG_VERSION"),
        registry.providers.len()
    );
    tracing::info!(
        "upstream: base={} key={}",
        upstream_base(),
        if upstream_key().is_empty() { "(none)" } else { "(set)" }
    );

    let app = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .fallback_service(tower::service_fn(|_req| async {
            Ok::<_, std::convert::Infallible>(
                StatusCode::NOT_FOUND,
            )
        }));

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port))
        .await
        .expect("failed to bind");
    tracing::info!("listening on http://localhost:{port}");
    axum::serve(listener, app).await.expect("server error");
}
