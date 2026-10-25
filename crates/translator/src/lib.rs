// OpenAI ↔ Claude message translation (request bodies, responses, and SSE
// chunks). Port of the 9router openai↔claude translator pair, scoped to the
// text-message surface (tool blocks pass through as-is inside `raw`).
//
// Rules carried over: preserve unknown fields, map finish reasons both ways,
// never invent content that the upstream did not send.
use serde_json::{json, Value};

/// Claude Messages request → OpenAI chat.completions request.
pub fn claude_request_to_openai(body: &Value) -> Value {
    let mut out = json!({});
    if let Some(model) = body.get("model") {
        out["model"] = model.clone();
    }
    if let Some(max) = body.get("max_tokens") {
        out["max_tokens"] = max.clone();
    }
    for key in ["temperature", "top_p", "stop"] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    // Claude: stop_sequences[] → OpenAI: stop[]
    if let Some(seqs) = body.get("stop_sequences").and_then(|v| v.as_array()) {
        out["stop"] = Value::Array(seqs.clone());
    }

    let mut messages: Vec<Value> = Vec::new();

    // system: string or content blocks → one (or more) system messages
    match body.get("system") {
        Some(Value::String(s)) => messages.push(json!({ "role": "system", "content": s })),
        Some(Value::Array(blocks)) => {
            let text = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                messages.push(json!({ "role": "system", "content": text }));
            }
        }
        _ => {}
    }

    if let Some(claude_msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in claude_msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = flatten_content(msg.get("content"));
            messages.push(json!({ "role": role, "content": content }));
        }
    }
    out["messages"] = Value::Array(messages);
    out
}

/// Claude content (string | blocks) → plain OpenAI text content.
/// Non-text blocks are kept as JSON markers so tool traffic stays debuggable.
fn flatten_content(content: Option<&Value>) -> Value {
    match content {
        Some(Value::String(s)) => Value::String(s.clone()),
        Some(Value::Array(blocks)) => {
            let mut parts: Vec<String> = Vec::new();
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("text") => {
                        if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                            parts.push(t.to_string());
                        }
                    }
                    Some("tool_use") => {
                        parts.push(format!(
                            "[tool_use {}]",
                            block.get("name").and_then(|n| n.as_str()).unwrap_or("")
                        ));
                    }
                    Some("tool_result") => {
                        let inner = flatten_content(block.get("content"));
                        parts.push(format!(
                            "[tool_result {inner}]"
                        ));
                    }
                    Some(other) => parts.push(format!("[{other} block]")),
                    None => {}
                }
            }
            Value::String(parts.join("\n"))
        }
        _ => Value::String(String::new()),
    }
}

/// Map OpenAI finish_reason → Claude stop_reason.
pub fn finish_reason_to_stop_reason(finish: Option<&str>) -> &'static str {
    match finish {
        Some("stop") | None | Some("end_turn") => "end_turn",
        Some("length") | Some("max_tokens") => "max_tokens",
        Some("tool_calls") | Some("function_call") | Some("tool_use") => "tool_use",
        Some("content_filter") | Some("refusal") => "refusal",
        Some("stop_sequence") => "stop_sequence",
        Some(_) => "end_turn",
    }
}

/// Map Claude stop_reason → OpenAI finish_reason.
pub fn stop_reason_to_finish_reason(stop: Option<&str>) -> &'static str {
    match stop {
        Some("max_tokens") => "length",
        Some("tool_use") => "tool_calls",
        _ => "stop",
    }
}

/// OpenAI chat.completions request → Claude Messages request (the inverse of
/// claude_request_to_openai; system messages fold into the system field).
pub fn openai_request_to_claude(body: &Value) -> Value {
    let mut out = json!({});
    if let Some(model) = body.get("model") {
        out["model"] = model.clone();
    }
    for key in ["temperature", "top_p"] {
        if let Some(v) = body.get(key) {
            out[key] = v.clone();
        }
    }
    if let Some(max) = body.get("max_tokens") {
        out["max_tokens"] = max.clone();
    } else {
        out["max_tokens"] = json!(4096); // Anthropic requires the field.
    }
    if let Some(stop) = body.get("stop") {
        if let Some(arr) = stop.as_array() {
            out["stop_sequences"] = Value::Array(arr.clone());
        } else if let Some(s) = stop.as_str() {
            out["stop_sequences"] = json!([s]);
        }
    }

    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::new();
    if let Some(openai_msgs) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in openai_msgs {
            let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
            let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or_default();
            if role == "system" {
                system_parts.push(content.to_string());
            } else {
                messages.push(json!({
                    "role": if role == "assistant" { "assistant" } else { "user" },
                    "content": content,
                }));
            }
        }
    }
    if !system_parts.is_empty() {
        out["system"] = Value::String(system_parts.join("\n"));
    }
    out["messages"] = Value::Array(messages);
    out
}

/// Claude message response → OpenAI chat.completion (non-stream).
pub fn claude_response_to_openai(body: &Value, model: &str) -> Value {
    let text = body
        .get("content")
        .and_then(|c| c.as_array())
        .map(|blocks| {
            blocks
                .iter()
                .filter_map(|b| {
                    if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                        b.get("text").and_then(|t| t.as_str()).map(String::from)
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("")
        })
        .unwrap_or_default();
    let stop = body.get("stop_reason").and_then(|s| s.as_str());
    let usage = body.get("usage").cloned().unwrap_or(json!({}));
    json!({
        "id": body.get("id").cloned().unwrap_or(json!("chatcmpl-gf")),
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": stop_reason_to_finish_reason(stop),
        }],
        "usage": {
            "prompt_tokens": usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "completion_tokens": usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "total_tokens": usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0)
                + usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        }
    })
}

/// OpenAI chat.completion (non-stream) → Claude message response.
pub fn openai_response_to_claude(body: &Value, model: &str) -> Value {
    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
        .cloned()
        .unwrap_or(json!({}));
    let message = choice.get("message").cloned().unwrap_or(json!({}));
    let text = message
        .get("content")
        .and_then(|c| c.as_str())
        .unwrap_or_default();
    let finish = choice.get("finish_reason").and_then(|f| f.as_str());
    let usage = body.get("usage").cloned().unwrap_or(json!({}));

    json!({
        "id": body.get("id").cloned().unwrap_or(Value::String(format!("msg_{}", crate_id()))),
        "type": "message",
        "role": "assistant",
        "model": model,
        "content": if text.is_empty() { Value::Array(vec![]) } else {
            json!([{ "type": "text", "text": text }])
        },
        "stop_reason": finish_reason_to_stop_reason(finish),
        "stop_sequence": Value::Null,
        "usage": {
            "input_tokens": usage.get("prompt_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
            "output_tokens": usage.get("completion_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
        }
    })
}

fn crate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().to_string())
        .unwrap_or_else(|_| "0".into())
}

// ── SSE streaming: OpenAI chunks → Claude events ──

/// Accumulator state for one streamed Claude message.
#[derive(Default)]
pub struct StreamState {
    pub started: bool,
    pub block_open: bool,
    pub text: String,
    pub finish_reason: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub model: String,
}

impl StreamState {
    pub fn new(model: &str) -> Self {
        StreamState {
            model: model.to_string(),
            ..Default::default()
        }
    }
}

/// Translate one parsed OpenAI stream chunk into zero or more Claude SSE
/// event payloads (without the `event:` line — the caller wraps).
pub fn openai_chunk_to_claude_events(chunk: &Value, state: &mut StreamState) -> Vec<Value> {
    let mut events = Vec::new();
    if !state.started {
        state.started = true;
        events.push(json!({
            "type": "message_start",
            "message": {
                "id": chunk.get("id").cloned().unwrap_or(json!("msg_stream")),
                "type": "message",
                "role": "assistant",
                "model": state.model,
                "content": [],
                "usage": { "input_tokens": 0, "output_tokens": 0 },
            }
        }));
    }

    let choices = chunk.get("choices").and_then(|c| c.as_array());
    let choice = choices.and_then(|c| c.first()).cloned().unwrap_or(json!({}));
    if let Some(delta) = choice.get("delta") {
        if let Some(text) = delta.get("content").and_then(|c| c.as_str()) {
            if !text.is_empty() {
                if !state.block_open {
                    state.block_open = true;
                    events.push(json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" }
                    }));
                }
                state.text.push_str(text);
                events.push(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": text }
                }));
            }
        }
    }
    if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        state.finish_reason = Some(finish.to_string());
    }
    if let Some(usage) = chunk.get("usage") {
        if let Some(v) = usage.get("prompt_tokens").and_then(|v| v.as_i64()) {
            state.input_tokens = v;
        }
        if let Some(v) = usage.get("completion_tokens").and_then(|v| v.as_i64()) {
            state.output_tokens = v;
        }
    }
    if state.finish_reason.is_some() {
        if state.block_open {
            state.block_open = false;
            events.push(json!({ "type": "content_block_stop", "index": 0 }));
        }
        events.push(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": finish_reason_to_stop_reason(state.finish_reason.as_deref()),
                "stop_sequence": Value::Null,
            },
            "usage": { "output_tokens": state.output_tokens }
        }));
        events.push(json!({ "type": "message_stop" }));
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_system_string_and_stop_sequences() {
        let claude = json!({
            "model": "zcode/glm-5.3-flash",
            "max_tokens": 64,
            "system": "You are helpful.",
            "stop_sequences": ["END"],
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] }
            ]
        });
        let openai = claude_request_to_openai(&claude);
        assert_eq!(openai["model"], "zcode/glm-5.3-flash");
        assert_eq!(openai["stop"], json!(["END"]));
        assert_eq!(openai["messages"][0]["role"], "system");
        assert_eq!(openai["messages"][1]["content"], "hi");
        assert_eq!(openai["messages"][2]["content"], "hello");
        assert_eq!(openai["messages"].as_array().unwrap().len(), 3);
    }

    #[test]
    fn response_mapping() {
        let openai = json!({
            "id": "chatcmpl-1",
            "choices": [{
                "message": { "role": "assistant", "content": "pong" },
                "finish_reason": "stop"
            }],
            "usage": { "prompt_tokens": 10, "completion_tokens": 1 }
        });
        let claude = openai_response_to_claude(&openai, "m");
        assert_eq!(claude["type"], "message");
        assert_eq!(claude["content"][0]["text"], "pong");
        assert_eq!(claude["stop_reason"], "end_turn");
        assert_eq!(claude["usage"]["input_tokens"], 10);
    }

    #[test]
    fn stream_chunk_state_machine() {
        let mut state = StreamState::new("m");
        let first = openai_chunk_to_claude_events(
            &json!({ "id": "c1", "choices": [{ "delta": { "content": "po" } }] }),
            &mut state,
        );
        assert_eq!(first[0]["type"], "message_start");
        assert_eq!(first[1]["type"], "content_block_start");
        assert_eq!(first[2]["type"], "content_block_delta");
        assert_eq!(first[2]["delta"]["text"], "po");

        let last = openai_chunk_to_claude_events(
            &json!({ "id": "c1", "choices": [{ "delta": {}, "finish_reason": "length" }], "usage": { "prompt_tokens": 5, "completion_tokens": 2 } }),
            &mut state,
        );
        let types: Vec<&str> = last.iter().filter_map(|e| e["type"].as_str()).collect();
        assert_eq!(types, vec!["content_block_stop", "message_delta", "message_stop"]);
        assert_eq!(last[1]["delta"]["stop_reason"], "max_tokens");
        assert_eq!(state.text, "po");
    }

    #[test]
    fn finish_reason_roundtrip() {
        assert_eq!(finish_reason_to_stop_reason(Some("length")), "max_tokens");
        assert_eq!(stop_reason_to_finish_reason(Some("max_tokens")), "length");
        assert_eq!(stop_reason_to_finish_reason(Some("end_turn")), "stop");
    }

    #[test]
    fn openai_request_to_claude_folds_system() {
        let openai = json!({
            "model": "glm/glm-5.3-flash",
            "max_tokens": 32,
            "stop": ["END"],
            "messages": [
                { "role": "system", "content": "be terse" },
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": "hello" },
                { "role": "user", "content": "bye" }
            ]
        });
        let claude = openai_request_to_claude(&openai);
        assert_eq!(claude["system"], "be terse");
        assert_eq!(claude["stop_sequences"], json!(["END"]));
        assert_eq!(claude["max_tokens"], 32);
        let msgs = claude["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn claude_response_to_openai_mapping() {
        let claude = json!({
            "id": "msg_1",
            "content": [
                { "type": "text", "text": "hello " },
                { "type": "text", "text": "world" },
                { "type": "tool_use", "id": "t1" }
            ],
            "stop_reason": "max_tokens",
            "usage": { "input_tokens": 7, "output_tokens": 3 }
        });
        let openai = claude_response_to_openai(&claude, "m");
        assert_eq!(openai["choices"][0]["message"]["content"], "hello world");
        assert_eq!(openai["choices"][0]["finish_reason"], "length");
        assert_eq!(openai["usage"]["prompt_tokens"], 7);
        assert_eq!(openai["usage"]["total_tokens"], 10);
    }
}
