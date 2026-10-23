// GateForge gateway core — exposed as a library so the Tauri app shell can
// embed the server; the `gateforge` binary is a thin wrapper over start_server.
use axum::{
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;

pub fn upstream_base() -> String {
    std::env::var("UPSTREAM_BASE_URL").unwrap_or_else(|_| "http://localhost:20228".into())
}

pub fn upstream_key() -> String {
    std::env::var("UPSTREAM_API_KEY").unwrap_or_default()
}

pub fn default_port() -> u16 {
    std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(20228)
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

async fn index() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "text/html; charset=utf-8")],
        concat!(
            "<!doctype html><html><head><title>GateForge</title>",
            "<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">",
            "<style>body{font-family:system-ui;background:#0b0b0d;color:#f4f4f5;",
            "display:grid;place-items:center;height:100vh;margin:0}",
            "main{text-align:center}a{color:#7dd3fc}</style></head><body><main>",
            "<h1>⚙ GateForge</h1><p>self-hosted AI gateway</p>",
            "<p><a href=\"/health\">health</a> · <a href=\"/v1/models\">models</a></p>",
            "</main></body></html>"
        ),
    )
}

/// Build the axum router (shared by the binary and the Tauri shell).
pub fn build_router() -> Router {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .fallback_service(tower::service_fn(|_req| async {
            Ok::<_, std::convert::Infallible>(StatusCode::NOT_FOUND)
        }))
}

/// Bind and serve. Resolves once the listener is bound (the serve future runs
/// for the lifetime of the spawned task).
pub async fn start_server(port: u16) -> std::io::Result<()> {
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

    let app = build_router();
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("listening on http://localhost:{port}");
    axum::serve(listener, app).await
}
