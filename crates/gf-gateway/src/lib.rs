// GateForge gateway core — exposed as a library so the Tauri app shell can
// embed the server; the `gateforge` binary is a thin wrapper over start_server.
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde_json::Value;
use std::sync::Arc;

type SharedStore = std::sync::Arc<std::sync::Mutex<gf_store::Store>>;

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

// Buffer an upstream SSE stream into one Claude message response (used when
// the client asked for non-stream JSON but the upstream forced SSE).
async fn sse_to_claude_response(
    response: reqwest::Response,
    model: &str,
) -> axum::response::Response {
    use futures_util::StreamExt;
    let bytes = response.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let mut state = gf_translator::StreamState::new(model);
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };
        if data == "[DONE]" {
            continue;
        }
        if let Ok(chunk) = serde_json::from_str::<Value>(data) {
            let _ = gf_translator::openai_chunk_to_claude_events(&chunk, &mut state);
        }
    }
    if !state.started {
        return (
            StatusCode::BAD_GATEWAY,
            "upstream stream carried no chunks",
        )
            .into_response();
    }
    let finish = state.finish_reason.as_deref();
    let content = if state.text.is_empty() {
        Value::Array(vec![])
    } else {
        serde_json::json!([{ "type": "text", "text": state.text }])
    };
    let claude = serde_json::json!({
        "id": format!("msg_gf_{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0)),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": content,
        "stop_reason": gf_translator::finish_reason_to_stop_reason(finish),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": state.input_tokens,
            "output_tokens": state.output_tokens,
        }
    });
    (StatusCode::OK, axum::Json(claude)).into_response()
}

/// Dashboard-facing: connections carried over from 9router (phase 2 read path).
async fn api_connections(State(store): State<SharedStore>) -> impl IntoResponse {
    let rows = {
        let store = store.lock().expect("store lock");
        store.list_connections(None).unwrap_or_default()
    };
    let data: Vec<Value> = rows
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "provider": c.provider,
                "authType": c.auth_type,
                "name": c.name,
                "email": c.email,
                "priority": c.priority,
                "isActive": c.is_active,
            })
        })
        .collect();
    Json(serde_json::json!({ "connections": data }))
}

/// Build the axum router (shared by the binary and the Tauri shell).
pub fn build_router() -> Router<SharedStore> {
    Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .route("/v1/messages", post(messages))
        .route("/api/connections", get(api_connections))
        .fallback_service(tower::service_fn(|_req| async {
            Ok::<_, std::convert::Infallible>(StatusCode::NOT_FOUND)
        }))
}

// Claude Messages endpoint: translate → upstream /v1/chat/completions →
// translate back (full JSON, or SSE events when stream=true).
async fn messages(
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let base = upstream_base();
    let key = upstream_key();
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    let stream = body.get("stream").and_then(|s| s.as_bool()).unwrap_or(false);

    let openai_body = gf_translator::claude_request_to_openai(&body);
    let client = reqwest::Client::new();
    let mut request = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&openai_body);
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
    // Forward the client's anthropic-version header for observability upstream.
    if let Some(version) = headers.get("anthropic-version") {
        if let Ok(v) = version.to_str() {
            request = request.header("anthropic-version", v);
        }
    }

    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let content_type = response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            if !stream {
                // Some upstreams force SSE regardless of stream:false — buffer
                // text/event-stream into a single completion and translate.
                if content_type.starts_with("text/event-stream") {
                    return sse_to_claude_response(response, &model).await;
                }
                match response.json::<Value>().await {
                    Ok(upstream_json) => {
                        let claude = gf_translator::openai_response_to_claude(
                            &upstream_json,
                            &model,
                        );
                        (StatusCode::OK, axum::Json(claude)).into_response()
                    }
                    Err(error) => (
                        StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY),
                        format!("upstream decode failed: {error}"),
                    )
                        .into_response(),
                }
            } else {
                // Translate the upstream SSE stream into Claude events.
                use futures_util::StreamExt;
                let mut state = gf_translator::StreamState::new(&model);
                let stream = response.bytes_stream();
                let mut buffer = String::new();
                let (tx, rx) = tokio::sync::mpsc::channel::<Result<axum::body::Bytes, std::convert::Infallible>>(32);
                tokio::spawn(async move {
                    let mut upstream = stream;
                    let mut state = state;
                    while let Some(chunk) = upstream.next().await {
                        let Ok(bytes) = chunk else { break };
                        buffer.push_str(&String::from_utf8_lossy(&bytes));
                        // Process complete SSE lines from the buffer.
                        while let Some(pos) = buffer.find('\n') {
                            let line: String = buffer.drain(..=pos).collect();
                            let line = line.trim();
                            if let Some(data) = line.strip_prefix("data: ") {
                                if data == "[DONE]" {
                                    continue;
                                }
                                if let Ok(chunk_json) = serde_json::from_str::<Value>(data) {
                                    for event in
                                        gf_translator::openai_chunk_to_claude_events(
                                            &chunk_json,
                                            &mut state,
                                        )
                                    {
                                        let payload = format!(
                                            "event: {}\ndata: {}\n\n",
                                            event["type"].as_str().unwrap_or("unknown"),
                                            event
                                        );
                                        let _ = tx
                                            .send(Ok(axum::body::Bytes::from(payload)))
                                            .await;
                                    }
                                }
                            }
                        }
                    }
                    // Upstream ended without finish_reason: close the Claude message.
                    if state.started && state.finish_reason.is_none() {
                        state.finish_reason = Some("stop".into());
                        let closing = vec![
                            serde_json::json!({ "type": "content_block_stop", "index": 0 }),
                            serde_json::json!({
                                "type": "message_delta",
                                "delta": { "stop_reason": "end_turn", "stop_sequence": Value::Null },
                                "usage": { "output_tokens": state.output_tokens }
                            }),
                            serde_json::json!({ "type": "message_stop" }),
                        ];
                        for event in closing {
                            let payload = format!(
                                "event: {}\ndata: {}\n\n",
                                event["type"].as_str().unwrap_or("unknown"),
                                event
                            );
                            let _ = tx.send(Ok(axum::body::Bytes::from(payload))).await;
                        }
                    }
                });
                let body_stream = tokio_stream::wrappers::ReceiverStream::new(rx);
                axum::response::Response::builder()
                    .status(StatusCode::OK)
                    .header("content-type", "text/event-stream")
                    .header("cache-control", "no-cache")
                    .body(axum::body::Body::from_stream(body_stream))
                    .unwrap()
            }
        }
        Err(error) => {
            let code = StatusCode::BAD_GATEWAY;
            (code, format!("upstream unreachable: {error}")).into_response()
        }
    }
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

    // Open the store (first boot copies a 9router database when present).
    let data_dir = gf_store::default_data_dir();
    let store = gf_store::Store::open(&data_dir).map_err(std::io::Error::other)?;
    let connections = store.list_connections(None).unwrap_or_default();
    tracing::info!(
        "store at {} — {} connections (migrated from 9router on first boot)",
        data_dir.display(),
        connections.len()
    );

    let app = build_router()
        .with_state(std::sync::Arc::new(std::sync::Mutex::new(store)));
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("listening on http://localhost:{port}");
    axum::serve(listener, app).await
}
