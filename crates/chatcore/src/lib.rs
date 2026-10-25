// Chat core: provider/model resolution, credential selection from the store,
// direct upstream dispatch and multi-account failover. This is where GateForge
// stops being a passthrough and becomes its own router.
use providers::{Provider, Registry, Transport};
use store::Store;
use serde_json::Value;

/// Split "provider/model" into its parts. None when there is no prefix.
pub fn split_model_id(model: &str) -> Option<(&str, &str)> {
    let (provider, model_id) = model.split_once('/')?;
    if provider.is_empty() || model_id.is_empty() {
        return None;
    }
    Some((provider, model_id))
}

/// A credential extracted from a connection's data JSON.
#[derive(Debug, Clone)]
pub struct Credential {
    pub connection_id: String,
    pub api_key: String,
}

/// Active credentials for a provider, priority order (failover sequence).
pub fn credentials_for(store: &Store, provider: &str) -> Vec<Credential> {
    store
        .list_connections(Some(provider))
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.is_active)
        .filter_map(|c| {
            let data: Value = serde_json::from_str(&c.data_json).unwrap_or(Value::Null);
            let key = data
                .get("apiKey")
                .or_else(|| data.get("accessToken"))
                .and_then(|k| k.as_str())
                .unwrap_or_default()
                .trim()
                .to_string();
            if key.is_empty() {
                None
            } else {
                Some(Credential {
                    connection_id: c.id,
                    api_key: key,
                })
            }
        })
        .collect()
}

/// Should this request be served directly (registry provider + credentials)?
pub fn direct_route(
    registry: &Registry,
    store: &Store,
    model: &str,
) -> Option<(Provider, String, Vec<Credential>)> {
    let (provider_id, model_id) = split_model_id(model)?;
    let provider = registry.get(provider_id)?.clone();
    let transport = provider.transport.clone()?;
    if transport.base_url.is_empty() {
        return None;
    }
    let creds = credentials_for(store, provider_id);
    if creds.is_empty() {
        return None;
    }
    Some((provider, model_id.to_string(), creds))
}

/// Apply the transport's auth scheme to a request builder.
fn apply_auth(
    mut request: reqwest::RequestBuilder,
    transport: &Transport,
    api_key: &str,
) -> reqwest::RequestBuilder {
    // Transport.auth shape (9router registry): { header, scheme: "raw"|"bearer" }
    let auth = transport.raw.get("auth").cloned().unwrap_or(Value::Null);
    let header = auth
        .get("header")
        .and_then(|h| h.as_str())
        .unwrap_or("Authorization");
    let scheme = auth
        .get("scheme")
        .and_then(|s| s.as_str())
        .unwrap_or("bearer");
    let value = match scheme {
        "raw" => api_key.to_string(),
        _ => format!("Bearer {api_key}"),
    };
    request = request.header(header, value);
    request
}

/// Dispatch one attempt against the provider upstream with a claude-format
/// body (9router's claude transports speak Anthropic Messages).
pub async fn dispatch_claude(
    transport: &Transport,
    credential: &Credential,
    model_id: &str,
    mut body: Value,
) -> Result<reqwest::Response, String> {
    body["model"] = Value::String(model_id.to_string());
    let mut url = transport.base_url.clone();
    if let Some(suffix) = transport.raw.get("urlSuffix").and_then(|s| s.as_str()) {
        url.push_str(suffix);
    }
    let mut request = reqwest::Client::new().post(&url).json(&body);
    for (name, value) in &transport.headers {
        request = request.header(name, value);
    }
    request = apply_auth(request, transport, &credential.api_key);
    request
        .send()
        .await
        .map_err(|error| format!("upstream unreachable: {error}"))
}

/// Failover outcome after trying every credential.
pub enum DispatchOutcome {
    Success {
        response: reqwest::Response,
        connection_id: String,
        attempts: usize,
    },
    Exhausted {
        status: Option<u16>,
        last_error: String,
        attempts: usize,
    },
}

/// Try each credential in order: 401/403 (bad account) and 429/5xx (busy
/// account) advance to the next one; anything else returns immediately.
pub async fn dispatch_with_failover(
    provider_id: &str,
    transport: &Transport,
    credentials: &[Credential],
    model_id: &str,
    body: &Value,
) -> DispatchOutcome {
    let mut attempts = 0;
    let mut last_error = String::from("no credentials");
    let mut last_status: Option<u16> = None;
    for credential in credentials {
        attempts += 1;
        match dispatch_claude(transport, credential, model_id, body.clone()).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let advance = matches!(status, 401 | 403 | 408 | 429) || status >= 500;
                if advance {
                    last_status = Some(status);
                    last_error = format!("HTTP {status}");
                    tracing::warn!(
                        provider = provider_id,
                        connection = credential.connection_id,
                        status,
                        "credential failed, trying next account"
                    );
                    continue;
                }
                return DispatchOutcome::Success {
                    response,
                    connection_id: credential.connection_id.clone(),
                    attempts,
                };
            }
            Err(error) => {
                last_error = error;
                continue;
            }
        }
    }
    DispatchOutcome::Exhausted {
        status: last_status,
        last_error,
        attempts,
    }
}

/// Dispatch one attempt against an OpenAI-compatible upstream.
pub async fn dispatch_openai(
    transport: &Transport,
    credential: &Credential,
    model_id: &str,
    mut body: Value,
) -> Result<reqwest::Response, String> {
    body["model"] = Value::String(model_id.to_string());
    let url = transport.base_url.clone();
    let mut request = reqwest::Client::new().post(&url).json(&body);
    for (name, value) in &transport.headers {
        request = request.header(name, value);
    }
    request = apply_auth(request, transport, &credential.api_key);
    request
        .send()
        .await
        .map_err(|error| format!("upstream unreachable: {error}"))
}

/// OpenAI-format failover (same advance rules as the claude path).
pub async fn dispatch_openai_with_failover(
    provider_id: &str,
    transport: &Transport,
    credentials: &[Credential],
    model_id: &str,
    body: &Value,
) -> DispatchOutcome {
    let mut attempts = 0;
    let mut last_error = String::from("no credentials");
    let mut last_status: Option<u16> = None;
    for credential in credentials {
        attempts += 1;
        match dispatch_openai(transport, credential, model_id, body.clone()).await {
            Ok(response) => {
                let status = response.status().as_u16();
                let advance = matches!(status, 401 | 403 | 408 | 429) || status >= 500;
                if advance {
                    last_status = Some(status);
                    last_error = format!("HTTP {status}");
                    tracing::warn!(
                        provider = provider_id,
                        connection = credential.connection_id,
                        status,
                        "credential failed, trying next account"
                    );
                    continue;
                }
                return DispatchOutcome::Success {
                    response,
                    connection_id: credential.connection_id.clone(),
                    attempts,
                };
            }
            Err(error) => {
                last_error = error;
                continue;
            }
        }
    }
    DispatchOutcome::Exhausted {
        status: last_status,
        last_error,
        attempts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_splitting() {
        assert_eq!(
            split_model_id("glm/glm-5.3-flash"),
            Some(("glm", "glm-5.3-flash"))
        );
        assert_eq!(split_model_id("glm-5.3-flash"), None);
        assert_eq!(split_model_id("/x"), None);
        assert_eq!(split_model_id("x/"), None);
    }

    #[test]
    fn credential_extraction_from_data_json() {
        let dir = std::env::temp_dir().join(format!("chatcore-test-{}", std::process::id()));
        std::env::set_var("GATEFORGE_NO_MIGRATE", "1");
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).expect("store");
        // Fixture keys are generated per-run — obviously synthetic, never real.
        let key_first = format!("fixture-key-first-{}", std::process::id());
        let key_second = format!("fixture-key-second-{}", std::process::id());
        let key_inactive = format!("fixture-key-inactive-{}", std::process::id());
        let row = |id: &str, priority: i64, active: bool, key: &str| store::ConnectionRow {
            id: id.into(),
            provider: "glm".into(),
            auth_type: "oauth".into(),
            name: None,
            email: None,
            priority: Some(priority),
            is_active: active,
            data_json: serde_json::json!({ "apiKey": key }).to_string(),
        };
        store.upsert_connection(&row("a1", 1, true, &key_first)).unwrap();
        store.upsert_connection(&row("a2", 2, true, &key_second)).unwrap();
        // inactive rows are skipped even with a key
        store
            .upsert_connection(&row("a3", 3, false, &key_inactive))
            .unwrap();
        let creds = credentials_for(&store, "glm");
        assert_eq!(creds.len(), 2);
        assert_eq!(creds[0].api_key, key_first);
        assert_eq!(creds[1].api_key, key_second);
        std::fs::remove_dir_all(&dir).ok();
    }
}
