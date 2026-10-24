// GateForge gateway core — exposed as a library so the Tauri app shell can
// embed the server; the `gateforge` binary is a thin wrapper over start_server.
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::IntoResponse,
    routing::{delete, get, post},
    Json, Router,
};
use chrono::Utc;
use gf_auth::machine_id;
use gf_store::Store;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

type SharedStore = Arc<Mutex<Store>>;

/// Success-path helper for the cross-format route: read a claude-format
/// upstream response (buffering SSE into one message) and emit an OpenAI
/// chat.completion. Exhausted outcomes map to their error response.
trait DirectClaudeToOpenai {
    async fn map_direct_claude_to_openai(self, model: &str) -> axum::response::Response;
}

impl DirectClaudeToOpenai for gf_chatcore::DispatchOutcome {
    async fn map_direct_claude_to_openai(self, model: &str) -> axum::response::Response {
        match self {
            gf_chatcore::DispatchOutcome::Success { response, attempts, .. } => {
                tracing::info!(model, attempts, "direct dispatch (claude→openai)");
                let status = StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);
                let content_type = response
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let claude_json = if content_type.starts_with("text/event-stream") {
                    match sse_buffer_to_claude_json(response, &model).await {
                        Some(json) => json,
                        None => {
                            return (StatusCode::BAD_GATEWAY, "upstream stream carried no events")
                                .into_response()
                        }
                    }
                } else {
                    match response.json::<Value>().await {
                        Ok(json) => json,
                        Err(error) => {
                            return (StatusCode::BAD_GATEWAY, format!("decode failed: {error}"))
                                .into_response()
                        }
                    }
                };
                let openai = gf_translator::claude_response_to_openai(&claude_json, model);
                (status, axum::Json(openai)).into_response()
            }
            gf_chatcore::DispatchOutcome::Exhausted { status, last_error, attempts } => {
                tracing::warn!(model, attempts, "all credentials exhausted");
                let code =
                    StatusCode::from_u16(status.unwrap_or(502)).unwrap_or(StatusCode::BAD_GATEWAY);
                (code, format!("all accounts failed: {last_error}")).into_response()
            }
        }
    }
}

/// Buffer a claude-format SSE stream into one claude message JSON.
async fn sse_buffer_to_claude_json(response: reqwest::Response, model: &str) -> Option<Value> {
    let bytes = response.bytes().await.unwrap_or_default();
    let text = String::from_utf8_lossy(&bytes);
    let mut message: Option<Value> = None;
    let mut blocks: Vec<Value> = Vec::new();
    let mut stop_reason = Value::Null;
    let mut input_tokens = 0i64;
    let mut output_tokens = 0i64;
    for line in text.lines() {
        let Some(data) = line.strip_prefix("data: ") else { continue };
        let Ok(event) = serde_json::from_str::<Value>(data) else { continue };
        match event.get("type").and_then(|t| t.as_str()) {
            Some("message_start") => {
                message = event.get("message").cloned();
                if let Some(tokens) = event.pointer("/message/usage/input_tokens").and_then(|v| v.as_i64()) {
                    input_tokens = tokens;
                }
            }
            Some("content_block_start") => {
                if event.get("content_block").and_then(|b| b.get("type")).and_then(|t| t.as_str())
                    == Some("text")
                {
                    blocks.push(serde_json::json!({ "type": "text", "text": "" }));
                }
            }
            Some("content_block_delta") => {
                if let Some(last) = blocks.last_mut() {
                    if let Some(delta_text) =
                        event.pointer("/delta/text").and_then(|t| t.as_str())
                    {
                        let current = last["text"].as_str().unwrap_or_default().to_string();
                        last["text"] = Value::String(current + delta_text);
                    }
                }
            }
            Some("message_delta") => {
                if let Some(stop) = event.pointer("/delta/stop_reason") {
                    stop_reason = stop.clone();
                }
                if let Some(tokens) = event.pointer("/usage/output_tokens").and_then(|v| v.as_i64())
                {
                    output_tokens = tokens;
                }
            }
            _ => {}
        }
    }
    let message = message?;
    Some(serde_json::json!({
        "id": message.get("id").cloned().unwrap_or(serde_json::json!("msg_gf")),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": blocks,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": { "input_tokens": input_tokens, "output_tokens": output_tokens },
    }))
}

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

// OpenAI chat.completions: direct dispatch when the model prefix has registry
// transport + active credentials (phase 3), else passthrough to the configured
// upstream (9router hop) with the model id kept verbatim.
async fn chat_completions(State(store): State<SharedStore>, Json(body): Json<Value>) -> impl IntoResponse {
    let model = body
        .get("model")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();

    {
        let registry = gf_providers::Registry::load_embedded();
        let route = {
            let store = store.lock().expect("store lock");
            gf_chatcore::direct_route(&registry, &store, &model)
        };
        if let Some((provider, model_id, creds)) = route {
            let transport = provider.transport.clone().unwrap();
            if transport.format == "claude" && !transport.base_url.starts_with("zcode://") {
                // Cross-format: client speaks OpenAI, provider speaks Claude.
                // Streamed claude responses are buffered into one completion.
                let claude_body = gf_translator::openai_request_to_claude(&body);
                let outcome = gf_chatcore::dispatch_with_failover(
                    &provider.id,
                    &transport,
                    &creds,
                    &model_id,
                    &claude_body,
                )
                .await;
                return match outcome {
                    gf_chatcore::DispatchOutcome::Success {
                        response, attempts, ..
                    } => {
                        tracing::info!(
                            provider = provider.id,
                            model = model_id,
                            attempts,
                            "direct dispatch (openai→claude buffered)"
                        );
                        // Upstream speaks claude; emit openai chat.completion.
                        let status = StatusCode::from_u16(response.status().as_u16())
                            .unwrap_or(StatusCode::OK);
                        let content_type = response
                            .headers()
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        let claude_json = if content_type.starts_with("text/event-stream") {
                            match sse_buffer_to_claude_json(response, &model).await {
                                Some(json) => json,
                                None => {
                                    return (
                                        StatusCode::BAD_GATEWAY,
                                        "upstream stream carried no events",
                                    )
                                        .into_response()
                                }
                            }
                        } else {
                            match response.json::<Value>().await {
                                Ok(json) => json,
                                Err(error) => {
                                    return (
                                        StatusCode::BAD_GATEWAY,
                                        format!("decode failed: {error}"),
                                    )
                                        .into_response()
                                }
                            }
                        };
                        let openai = gf_translator::claude_response_to_openai(&claude_json, &model);
                        (status, axum::Json(openai)).into_response()
                    }
                    gf_chatcore::DispatchOutcome::Exhausted {
                        status,
                        last_error,
                        attempts,
                    } => {
                        tracing::warn!(provider = provider.id, attempts, "all credentials exhausted");
                        let code = StatusCode::from_u16(status.unwrap_or(502))
                            .unwrap_or(StatusCode::BAD_GATEWAY);
                        (code, format!("all accounts failed: {last_error}")).into_response()
                    }
                };
            }
            if transport.format != "claude" && !transport.base_url.starts_with("zcode://") {
                return match gf_chatcore::dispatch_openai_with_failover(
                    &provider.id,
                    &transport,
                    &creds,
                    &model_id,
                    &body,
                )
                .await
                {
                    gf_chatcore::DispatchOutcome::Success {
                        response, attempts, ..
                    } => {
                        tracing::info!(
                            provider = provider.id,
                            model = model_id,
                            attempts,
                            "direct dispatch (openai)"
                        );
                        let status = StatusCode::from_u16(response.status().as_u16())
                            .unwrap_or(StatusCode::OK);
                        match response.text().await {
                            Ok(text) => (status, text).into_response(),
                            Err(error) => (
                                StatusCode::BAD_GATEWAY,
                                format!("direct upstream read failed: {error}"),
                            )
                                .into_response(),
                        }
                    }
                    gf_chatcore::DispatchOutcome::Exhausted {
                        status,
                        last_error,
                        attempts,
                    } => {
                        tracing::warn!(provider = provider.id, attempts, "all credentials exhausted");
                        let code = StatusCode::from_u16(status.unwrap_or(502))
                            .unwrap_or(StatusCode::BAD_GATEWAY);
                        (code, format!("all accounts failed: {last_error}")).into_response()
                    }
                };
            }
        }
    }

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
        .route("/api/keys", get(list_keys).post(create_key))
        .route("/api/keys/{id}", axum::routing::delete(delete_key))
        .route("/api/auth/login", post(auth_login))
        .route("/api/auth/status", get(auth_status))
        .fallback_service(tower::service_fn(|_req| async {
            Ok::<_, std::convert::Infallible>(StatusCode::NOT_FOUND)
        }))
}

// Claude Messages endpoint: translate → upstream /v1/chat/completions →
// translate back (full JSON, or SSE events when stream=true).
async fn messages(
    State(store): State<SharedStore>,
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

    // Direct route (phase 3/4): registry provider + migrated credentials →
    // dispatch straight to the provider upstream with failover; no 9router hop.
    {
        let registry = gf_providers::Registry::load_embedded();
        let route = {
            let store = store.lock().expect("store lock");
            gf_chatcore::direct_route(&registry, &store, &model)
        };
        if let Some((provider, model_id, creds)) = route {
            let transport = provider.transport.clone().unwrap();
            if transport.format == "claude" && !transport.base_url.starts_with("zcode://") {
                // Same format: dispatch the claude body verbatim.
                return match gf_chatcore::dispatch_with_failover(
                    &provider.id,
                    &transport,
                    &creds,
                    &model_id,
                    &body,
                )
                .await
                {
                    gf_chatcore::DispatchOutcome::Success {
                        response, attempts, ..
                    } => {
                        tracing::info!(
                            provider = provider.id,
                            model = model_id,
                            attempts,
                            "direct dispatch (claude)"
                        );
                        // claude→claude: passthrough the provider response.
                        let status =
                            StatusCode::from_u16(response.status().as_u16()).unwrap_or(StatusCode::OK);
                        match response.text().await {
                            Ok(text) => (status, text).into_response(),
                            Err(error) => (
                                StatusCode::BAD_GATEWAY,
                                format!("direct upstream read failed: {error}"),
                            )
                                .into_response(),
                        }
                    }
                    gf_chatcore::DispatchOutcome::Exhausted {
                        status,
                        last_error,
                        attempts,
                    } => {
                        tracing::warn!(
                            provider = provider.id,
                            attempts,
                            "all credentials exhausted"
                        );
                        let code = StatusCode::from_u16(status.unwrap_or(502))
                            .unwrap_or(StatusCode::BAD_GATEWAY);
                        (code, format!("all accounts failed: {last_error}")).into_response()
                    }
                };
            }
            // Cross-format: client speaks Claude, provider speaks OpenAI.
            if transport.format != "claude" && !transport.base_url.starts_with("zcode://") {
                let openai_body = gf_translator::claude_request_to_openai(&body);
                return match gf_chatcore::dispatch_openai_with_failover(
                    &provider.id,
                    &transport,
                    &creds,
                    &model_id,
                    &openai_body,
                )
                .await
                {
                    gf_chatcore::DispatchOutcome::Success {
                        response, attempts, ..
                    } => {
                        tracing::info!(
                            provider = provider.id,
                            model = model_id,
                            attempts,
                            "direct dispatch (openai→claude)"
                        );
                        let content_type = response
                            .headers()
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        if content_type.starts_with("text/event-stream") {
                            // Reuse the SSE buffering path used for the
                            // passthrough route (openai chunks → claude JSON).
                            return sse_to_claude_response(response, &model).await;
                        }
                        match response.json::<Value>().await {
                            Ok(openai_json) => {
                                let claude = gf_translator::openai_response_to_claude(
                                    &openai_json,
                                    &model,
                                );
                                (StatusCode::OK, axum::Json(claude)).into_response()
                            }
                            Err(error) => (
                                StatusCode::BAD_GATEWAY,
                                format!("direct upstream decode failed: {error}"),
                            )
                                .into_response(),
                        }
                    }
                    gf_chatcore::DispatchOutcome::Exhausted {
                        status,
                        last_error,
                        attempts,
                    } => {
                        tracing::warn!(
                            provider = provider.id,
                            attempts,
                            "all credentials exhausted"
                        );
                        let code = StatusCode::from_u16(status.unwrap_or(502))
                            .unwrap_or(StatusCode::BAD_GATEWAY);
                        (code, format!("all accounts failed: {last_error}")).into_response()
                    }
                };
            }
        }
    }

    let base = upstream_base();
    let key = upstream_key();
    let openai_body = gf_translator::claude_request_to_openai(&body);
    let client = reqwest::Client::new();
    let mut request = client
        .post(format!("{base}/v1/chat/completions"))
        .json(&openai_body);
    if !key.is_empty() {
        request = request.bearer_auth(key);
    }
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
    let store = Store::open(&data_dir).map_err(std::io::Error::other)?;
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

// ── Dashboard auth + API key management handlers ──

/// POST /api/auth/login — verify password, mint a session token.
async fn auth_login(
    State(store): State<SharedStore>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let password = body.get("password").and_then(|p| p.as_str()).unwrap_or("");
    let settings = {
        let store = store.lock().expect("store lock");
        store.get_settings()
    };
    let settings_json: Value = settings
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json_null());
    let stored = settings_json
        .get("password")
        .and_then(|p| p.as_str())
        .unwrap_or("");
    let require = settings_json
        .get("requireLogin")
        .and_then(|r| r.as_bool())
        .unwrap_or(true);

    // First boot: no password stored → accept the default or any password.
    if !stored.is_empty() && stored != password {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "Invalid password" })),
        )
            .into_response();
    }
    if stored.is_empty() && require && password != "123456" {
        return (
            StatusCode::UNAUTHORIZED,
            axum::Json(serde_json::json!({ "error": "Invalid password" })),
        )
            .into_response();
    }

    // Mint a session token (HS256-like: HMAC of machine id + expiry).
    let auth = gf_auth::machine_id();
    let token = gf_auth::dashboard_session_token(&auth, password);
    (
        [(header::SET_COOKIE, format!("auth_token={token}; Path=/; HttpOnly"))],
        axum::Json(serde_json::json!({ "ok": true })),
    )
        .into_response()
}

/// GET /api/auth/status — requireLogin + whether a password is set.
async fn auth_status(State(store): State<SharedStore>) -> impl IntoResponse {
    let settings = {
        let store = store.lock().expect("store lock");
        store.get_settings()
    };
    let settings_json: Value = settings
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(json_null());
    let require = settings_json
        .get("requireLogin")
        .and_then(|r| r.as_bool())
        .unwrap_or(true);
    let has_password = settings_json
        .get("password")
        .and_then(|p| p.as_str())
        .is_some_and(|p| !p.is_empty());
    axum::Json(serde_json::json!({
        "requireLogin": require,
        "authMode": "password",
        "hasPassword": has_password,
    }))
}

/// GET /api/keys — list API keys.
async fn list_keys(State(store): State<SharedStore>) -> impl IntoResponse {
    let keys = {
        let store = store.lock().expect("store lock");
        store.list_api_keys().unwrap_or_default()
    };
    let data: Vec<Value> = keys
        .iter()
        .map(|k| {
            serde_json::json!({
                "id": k.id, "key": k.key, "name": k.name, "isActive": k.is_active,
            })
        })
        .collect();
    axum::Json(serde_json::json!({ "keys": data }))
}

/// POST /api/keys — create a new API key (sk-{machineId}-{keyId}-{crc8}).
async fn create_key(
    State(store): State<SharedStore>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let name = body
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("unnamed")
        .to_string();
    let machine = machine_id();
    let generated = gf_auth::generate_api_key(&machine);
    let id = gf_auth::uuid_v4();
    let now = chrono::Utc::now().to_rfc3339();
    {
        let store = store.lock().expect("store lock");
        if let Err(error) = store
            .upsert_api_key(&gf_store::ApiKeyRow {
                id: id.clone(),
                key: generated.key.clone(),
                name: Some(name.clone()),
                is_active: true,
            })
        {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("key persist failed: {error}"),
            )
                .into_response();
        }
    }
    tracing::info!(name, "API key created");
    (
        StatusCode::CREATED,
        axum::Json(serde_json::json!({
            "id": id, "key": generated.key, "name": name, "machineId": machine,
        })),
    )
        .into_response()
}

/// DELETE /api/keys/{id} — remove an API key.
async fn delete_key(
    State(store): State<SharedStore>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let deleted = {
        let store = store.lock().expect("store lock");
        match store.delete_api_key(&id) {
            Ok(count) => count,
            Err(error) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("{error}"))
                    .into_response()
            }
        }
    };
    if deleted == 0 {
        return (StatusCode::NOT_FOUND, "key not found").into_response();
    }
    StatusCode::NO_CONTENT.into_response()
}

fn json_null() -> Value {
    Value::Object(serde_json::Map::new())
}
