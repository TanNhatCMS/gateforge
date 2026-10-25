// ZCode bridge — drives the locally installed ZCode runtime so model calls are
// made by the genuine ZCode process (Start Plan quota works without
// reproducing the app's WAF/captcha material). No Z.ai credential is stored —
// the shared local profile owns auth.
//
// Two spawn modes, ported from 9router open-sse/executors/zcode*.js:
//  - server dist (~/.zcode/server/zcode-server.cjs, preferred): JSON hello line
//    then 13-byte length-prefixed binary frames; self-manages account
//    providers, so Start Plan resolves without a desktop host.
//  - desktop CLI app-server (`zcode.cjs app-server`, fallback): NDJSON
//    JSON-RPC; needs the Start Plan selection pinned into the shared personal
//    provider config because the worker registry has no account sync.
//
// The protocol client itself runs in node (assets/zcode-bridge-client.cjs):
// the server dist closes its stdio mid-handshake when the client process is
// not node — verified against node clients with byte-identical frames
// succeeding where the Rust client got EOF. GateForge therefore exchanges one
// JSON-line request/result with the bridge client and keeps everything else
// (gateway, store, translation, failover) native. The binary frame codec below
// stays as the tested wire-format reference for a future native client.
use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64URL;
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Constants & env helpers (same defaults as the 9router executor)
// ---------------------------------------------------------------------------

const HEADER_SIZE: usize = 13;
const REGULAR_MESSAGE: u8 = 1;
const INITIALIZE_MESSAGE: u8 = 200;
const RESPONSE_MESSAGE: u8 = 201;
const ERROR_MESSAGE: u8 = 202;
const CANCELED_MESSAGE: u8 = 203;
const MAX_FRAME_BYTES: u32 = 32 * 1024 * 1024;

// Start Plan serves GLM-5.3 / GLM-5.3-Flash under this account provider.
const DEFAULT_AGENT_PROVIDER_ID: &str = "account:zai-start-plan";

fn env_num(name: &str, fallback: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(fallback)
}

fn env_flag(name: &str, default: bool) -> bool {
    match std::env::var(name).ok().as_deref() {
        Some("true") | Some("1") => true,
        Some("false") | Some("0") => false,
        _ => default,
    }
}

fn turn_timeout_ms() -> u64 {
    env_num("ZCODE_TURN_TIMEOUT_MS", 300_000)
}
fn poll_interval_ms() -> u64 {
    env_num("ZCODE_POLL_INTERVAL_MS", 2_000)
}
fn startup_timeout_ms() -> u64 {
    env_num("ZCODE_STARTUP_TIMEOUT_MS", 30_000)
}
fn rpc_timeout_ms() -> u64 {
    env_num("ZCODE_RPC_TIMEOUT_MS", 60_000)
}
fn agent_provider_id() -> String {
    std::env::var("ZCODE_AGENT_PROVIDER_ID")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_AGENT_PROVIDER_ID.to_string())
}

// ---------------------------------------------------------------------------
// Compact value codec (server-dist binary protocol) — wire-format reference
// ---------------------------------------------------------------------------
// Wire encoding from zcode-server-protocol.js: one type byte then payload.
// 0=undefined, 1=utf8 string, 2=bytes, 4=array, 5=JSON document, 6=u64.
// Lengths are variable-length quantities (7 bits per byte, little-endian).

pub fn encode_vql(mut value: u64, out: &mut Vec<u8>) {
    loop {
        let mut next = (value % 128) as u8;
        value /= 128;
        if value > 0 {
            next |= 0x80;
        }
        out.push(next);
        if value == 0 {
            break;
        }
    }
}

pub fn decode_vql(data: &[u8], offset: &mut usize) -> Result<u64, String> {
    let mut value: u64 = 0;
    let mut multiplier: u64 = 1;
    for _ in 0..8 {
        if *offset >= data.len() {
            return Err("Truncated ZCode variable-length quantity".into());
        }
        let next = data[*offset];
        *offset += 1;
        value += u64::from(next & 0x7f) * multiplier;
        if next & 0x80 == 0 {
            return Ok(value);
        }
        multiplier *= 128;
    }
    Err("Invalid ZCode variable-length quantity".into())
}

pub fn encode_zcode_value(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Null => out.push(0),
        Value::String(text) => {
            out.push(1);
            encode_vql(text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        Value::Array(items) => {
            out.push(4);
            encode_vql(items.len() as u64, out);
            for item in items {
                encode_zcode_value(item, out);
            }
        }
        // Safe non-negative integers take the compact form; everything else
        // (objects, bools, floats, negatives) rides as a JSON document.
        Value::Number(number) => {
            if let Some(unsigned) = number.as_u64() {
                out.push(6);
                encode_vql(unsigned, out);
            } else {
                encode_json_value(value, out);
            }
        }
        other => encode_json_value(other, out),
    }
}

fn encode_json_value(value: &Value, out: &mut Vec<u8>) {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    out.push(5);
    encode_vql(bytes.len() as u64, out);
    out.extend_from_slice(&bytes);
}

pub fn decode_zcode_value(data: &[u8], offset: &mut usize) -> Result<Value, String> {
    if *offset >= data.len() {
        return Err("Truncated ZCode serialized value".into());
    }
    let kind = data[*offset];
    *offset += 1;
    match kind {
        0 => Ok(Value::Null),
        1 | 2 => {
            let length = decode_vql(data, offset)?;
            let end = *offset + length as usize;
            if end > data.len() {
                return Err("Truncated ZCode byte/string value".into());
            }
            let text = String::from_utf8_lossy(&data[*offset..end]).into_owned();
            *offset = end;
            Ok(Value::String(text))
        }
        4 => {
            let length = decode_vql(data, offset)? as usize;
            let mut items = Vec::with_capacity(length.min(1024));
            for _ in 0..length {
                items.push(decode_zcode_value(data, offset)?);
            }
            Ok(Value::Array(items))
        }
        5 => {
            let length = decode_vql(data, offset)? as usize;
            let end = *offset + length;
            if end > data.len() {
                return Err("Truncated ZCode JSON value".into());
            }
            let parsed = serde_json::from_slice(&data[*offset..end])
                .map_err(|error| format!("Invalid ZCode JSON value: {error}"))?;
            *offset = end;
            Ok(parsed)
        }
        6 => {
            let value = decode_vql(data, offset)?;
            Ok(json!(value))
        }
        other => Err(format!("Unknown ZCode serialized value type {other}")),
    }
}

pub fn encode_rpc_call(id: u64, channel: &str, method: &str, args: &Value) -> Vec<u8> {
    let mut body = Vec::new();
    encode_zcode_value(&json!([100, id, channel, method]), &mut body);
    encode_zcode_value(args, &mut body);
    let mut frame = vec![0u8; HEADER_SIZE + body.len()];
    frame[0] = REGULAR_MESSAGE;
    frame[9..HEADER_SIZE].copy_from_slice(&(body.len() as u32).to_be_bytes());
    frame[HEADER_SIZE..].copy_from_slice(&body);
    frame
}

// Frame kind markers used by the bridge client; kept here so the constants and
// the codec tell the whole wire story in one place.
pub const FRAME_KINDS: (u8, u8, u8, u8, u8) = (
    REGULAR_MESSAGE,
    INITIALIZE_MESSAGE,
    RESPONSE_MESSAGE,
    ERROR_MESSAGE,
    CANCELED_MESSAGE,
);
pub const FRAME_MAX_BYTES: u32 = MAX_FRAME_BYTES;

// ---------------------------------------------------------------------------
// Process plumbing (spawning the node bridge client)
// ---------------------------------------------------------------------------

struct SpawnTarget {
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
}

struct ChildProcess {
    child: Child,
    stdin: ChildStdin,
    reader: BufReader<ChildStdout>,
    stderr_head: Arc<Mutex<String>>,
}

fn resolve_node() -> String {
    for name in ["ZCODE_NODE", "ZCODE_SERVER_NODE"] {
        if let Ok(node) = std::env::var(name) {
            if !node.trim().is_empty() {
                return node;
            }
        }
    }
    // Absolute path: the JS host passed process.execPath, and PATH search for
    // a bare "node" is fragile inside services.
    if let Ok(dirs) = std::env::var("PATH") {
        let exe = if cfg!(windows) { "node.exe" } else { "node" };
        for dir in dirs.split(if cfg!(windows) { ';' } else { ':' }) {
            let candidate = PathBuf::from(dir).join(exe);
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "node".to_string()
}

async fn spawn_child(target: &SpawnTarget) -> Result<ChildProcess, String> {
    if target.command.is_empty()
        || target.command.contains(['\r', '\n'])
        || target.args.iter().any(|arg| arg.contains(['\r', '\n']))
    {
        return Err("Invalid ZCode spawn target".into());
    }
    let mut cmd = Command::new(&target.command);
    cmd.args(&target.args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    for (key, value) in &target.env {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().map_err(|error| {
        format!(
            "failed to spawn {} {}: {error}",
            target.command,
            target.args.join(" ")
        )
    })?;
    let stdin = child.stdin.take().ok_or("no stdin on zcode child")?;
    let stdout = child.stdout.take().ok_or("no stdout on zcode child")?;
    let stderr = child.stderr.take().ok_or("no stderr on zcode child")?;
    let stderr_head: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
    {
        // Child stderr may carry provider diagnostics or credentials; keep a
        // short head for exit diagnostics only, never forward it.
        let head = stderr_head.clone();
        tokio::spawn(async move {
            let mut stderr = stderr;
            let mut acc = String::new();
            let mut chunk = [0u8; 512];
            loop {
                match stderr.read(&mut chunk).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        acc.push_str(&String::from_utf8_lossy(&chunk[..n]));
                        if acc.len() >= 2000 {
                            break;
                        }
                    }
                }
            }
            *head.lock().await = acc;
        });
    }
    Ok(ChildProcess {
        child,
        stdin,
        reader: BufReader::new(stdout),
        stderr_head,
    })
}

impl ChildProcess {
    async fn write_line(&mut self, bytes: &[u8]) -> Result<(), String> {
        let mut payload = bytes.to_vec();
        payload.push(b'\n');
        self.stdin
            .write_all(&payload)
            .await
            .map_err(|error| format!("bridge stdin write failed: {error}"))
    }

    // One newline-terminated line — the bridge client's JSON result.
    async fn read_line(&mut self) -> Result<String, String> {
        let mut line = String::new();
        self.reader
            .read_line(&mut line)
            .await
            .map_err(|error| error.to_string())?;
        if line.trim().is_empty() {
            let head = self.stderr_head.lock().await.clone();
            let tail: Vec<&str> = head.trim().lines().rev().take(3).collect();
            let tail = tail.into_iter().rev().collect::<Vec<_>>().join(" | ");
            return Err(if tail.is_empty() {
                "zcode bridge client exited without a result".to_string()
            } else {
                format!(
                    "zcode bridge client exited — {}",
                    tail.chars().take(400).collect::<String>()
                )
            });
        }
        Ok(line.trim().to_string())
    }

    async fn kill(&mut self) {
        let _ = self.stdin.shutdown().await;
        let _ = self.child.kill().await;
    }
}

// ---------------------------------------------------------------------------
// Spawn-target resolution (server dist preferred, desktop CLI fallback)
// ---------------------------------------------------------------------------

fn resolve_server_dist_command() -> Option<SpawnTarget> {
    let (command, server_entry) = if let (Ok(node), Ok(entry)) = (
        std::env::var("ZCODE_SERVER_NODE"),
        std::env::var("ZCODE_SERVER_ENTRY"),
    ) {
        if node.trim().is_empty() || entry.trim().is_empty() {
            return None;
        }
        (node, entry)
    } else {
        let runtime_root = std::env::var("ZCODE_SERVER_RUNTIME_ROOT").unwrap_or_else(|_| {
            home_dir()
                .join(".zcode")
                .join("server")
                .to_string_lossy()
                .into_owned()
        });
        let server_entry = PathBuf::from(&runtime_root).join("zcode-server.cjs");
        if !server_entry.exists() {
            return None;
        }
        (
            resolve_node(),
            server_entry.to_string_lossy().into_owned(),
        )
    };
    let mut target = SpawnTarget {
        command,
        args: vec![server_entry.clone()],
        env: BTreeMap::new(),
    };
    // The dist is a supervisor: it spawns the agent itself using bundled-agent
    // wiring env — point that wiring at the installed desktop CLI bundle.
    let entry_dir = PathBuf::from(&server_entry)
        .parent()
        .map(PathBuf::from)
        .unwrap_or_default();
    if let Some((wiring_command, args_json)) = resolve_bundled_agent_wiring(&entry_dir) {
        target
            .env
            .insert("ZCODE_AGENT_SERVER_COMMAND".into(), wiring_command);
        target
            .env
            .insert("ZCODE_AGENT_SERVER_ARGS_JSON".into(), args_json);
    }
    Some(target)
}

fn resolve_bundled_agent_wiring(entry_dir: &PathBuf) -> Option<(String, String)> {
    if let Ok(command) = std::env::var("ZCODE_AGENT_SERVER_COMMAND") {
        if !command.trim().is_empty() {
            let args = std::env::var("ZCODE_AGENT_SERVER_ARGS_JSON")
                .ok()
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_else(|| vec!["app-server".into(), "--stdio".into()]);
            return Some((command, serde_json::to_string(&args).unwrap_or_default()));
        }
    }
    let mut candidates = vec![entry_dir.join("zcode.cjs")];
    if cfg!(windows) {
        candidates.push(
            PathBuf::from(
                std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into()),
            )
            .join("ZCode")
            .join("resources")
            .join("glm")
            .join("zcode.cjs"),
        );
    } else if cfg!(target_os = "macos") {
        candidates.push(PathBuf::from(
            "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs",
        ));
    } else {
        candidates.push(PathBuf::from("/opt/ZCode/resources/glm/zcode.cjs"));
    }
    for candidate in candidates {
        if candidate.exists() {
            return Some((
                resolve_node(),
                serde_json::to_string(&vec![
                    candidate.to_string_lossy().into_owned(),
                    "app-server".to_string(),
                    "--stdio".to_string(),
                ])
                .unwrap_or_default(),
            ));
        }
    }
    None
}

fn auto_detect_desktop_cli() -> Option<PathBuf> {
    let candidates: Vec<PathBuf> = if cfg!(windows) {
        vec![
            PathBuf::from(
                std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".into()),
            )
            .join("ZCode")
            .join("resources")
            .join("glm")
            .join("zcode.cjs"),
            PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default())
                .join("Programs")
                .join("ZCode")
                .join("resources")
                .join("glm")
                .join("zcode.cjs"),
        ]
    } else if cfg!(target_os = "macos") {
        vec![PathBuf::from(
            "/Applications/ZCode.app/Contents/Resources/glm/zcode.cjs",
        )]
    } else {
        vec![
            PathBuf::from("/opt/ZCode/resources/glm/zcode.cjs"),
            PathBuf::from("/usr/lib/ZCode/resources/glm/zcode.cjs"),
        ]
    };
    candidates.into_iter().find(|candidate| candidate.exists())
}

fn resolve_app_server_command() -> SpawnTarget {
    if let Ok(cjs) = std::env::var("ZCODE_CJS") {
        if !cjs.trim().is_empty() {
            return SpawnTarget {
                command: resolve_node(),
                args: vec![cjs, "app-server".into()],
                env: BTreeMap::new(),
            };
        }
    }
    if let Ok(bin) = std::env::var("ZCODE_BIN") {
        if !bin.trim().is_empty() {
            let args = std::env::var("ZCODE_ARGS")
                .ok()
                .and_then(|raw| serde_json::from_str::<Vec<String>>(&raw).ok())
                .unwrap_or_else(|| vec!["app-server".into()]);
            return SpawnTarget {
                command: bin,
                args,
                env: BTreeMap::new(),
            };
        }
    }
    if let Some(cli) = auto_detect_desktop_cli() {
        return SpawnTarget {
            command: resolve_node(),
            args: vec![cli.to_string_lossy().into_owned(), "app-server".into()],
            env: BTreeMap::new(),
        };
    }
    SpawnTarget {
        command: "zcode".into(),
        args: vec!["app-server".into()],
        env: BTreeMap::new(),
    }
}

// ---------------------------------------------------------------------------
// Provider-config env for the app-server worker (Start Plan pinning)
// ---------------------------------------------------------------------------

fn home_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(
            std::env::var("USERPROFILE").unwrap_or_else(|_| "C:\\Users\\unknown".into()),
        )
    } else {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
    }
}

// Bridge-owned workspace directory: bridge sessions stay isolated from the
// caller's cwd and whatever the ZCode app has selected there.
fn workspace_path() -> String {
    let workspace = store::default_data_dir()
        .join("zcode-bridge")
        .join("workspace");
    let _ = std::fs::create_dir_all(&workspace);
    workspace.to_string_lossy().into_owned()
}

// The CLI registry resolves account providers from these files; pointing the
// worker at the same store the desktop uses keeps the model catalog in sync.
fn discover_builtin_provider_config() -> Option<PathBuf> {
    let platform = if cfg!(windows) {
        "windows"
    } else if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    // std::env::consts::ARCH is "x86_64"/"aarch64" — the same naming the
    // ZCode runtime uses for its provider dirs.
    let arch = std::env::consts::ARCH;
    let root = home_dir()
        .join(".zcode")
        .join("v2")
        .join("runtime")
        .join("provider")
        .join(format!("{platform}-{arch}"));
    let versions = std::fs::read_dir(&root).ok()?;
    let mut version_dirs: Vec<(u64, u64, u64, PathBuf)> = versions
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let parts: Vec<u64> = name.split('.').filter_map(|p| p.parse().ok()).collect();
            if parts.len() == 3 {
                Some((parts[0], parts[1], parts[2], entry.path()))
            } else {
                None
            }
        })
        .collect();
    version_dirs.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)).then(b.2.cmp(&a.2)));
    for (_, _, _, version_dir) in version_dirs {
        let endpoints = std::fs::read_dir(&version_dir).ok()?;
        let mut endpoint_names: Vec<String> = endpoints
            .flatten()
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|name| name.starts_with("endpoint-"))
            .collect();
        endpoint_names.sort();
        for endpoint in endpoint_names {
            let candidate = version_dir.join(endpoint).join("zcode-builtin.json");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

// Copy the personal provider config into the data dir, keeping only
// ZCode-managed provider rules (builtin:*/account:*) and injecting the wanted
// default selection. Custom providers (e.g. one pointing back at this router)
// would shadow the Start Plan account provider — the copy restores it without
// touching the user's real desktop config.
fn build_bridge_personal_config(
    personal_file: &PathBuf,
    default_selection: &Value,
) -> Option<PathBuf> {
    let mut raw: Value = serde_json::from_slice(&std::fs::read(personal_file).ok()?).ok()?;
    let config = raw
        .as_object_mut()?
        .entry("config")
        .or_insert_with(|| json!({}));
    for rules_key in ["providerConfigRules", "modelConfigRules"] {
        if let Some(rules) = config
            .pointer_mut(&format!("/{rules_key}/providerRules"))
            .and_then(|r| r.as_array_mut())
        {
            rules.retain(|rule| {
                let id = rule
                    .get("providerId")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                id.starts_with("builtin:") || id.starts_with("account:")
            });
        }
    }
    config["defaultModelSelection"] = default_selection.clone();
    let dir = store::default_data_dir().join("zcode-bridge");
    std::fs::create_dir_all(&dir).ok()?;
    let target = dir.join("personal.provider_config.json");
    let tmp = dir.join("personal.provider_config.json.tmp");
    let body = serde_json::to_string_pretty(&raw).ok()?;
    std::fs::write(&tmp, body + "\n").ok()?;
    std::fs::rename(&tmp, &target).ok()?;
    Some(target)
}

// The worker's default selection is the ONLY model path that resolves reliably
// (the worker registry has no account sync). Pin the shared personal config's
// defaultModelSelection to the Start Plan model so the worker — and the ZCode
// app reading the same file — serve Start Plan.
fn ensure_start_plan_default_selection(personal_file: &PathBuf, upstream_model: &str) {
    let wanted = json!({
        "providerId": agent_provider_id(),
        "modelId": upstream_model,
    });
    let Ok(raw_bytes) = std::fs::read(personal_file) else {
        return;
    };
    let Ok(mut raw) = serde_json::from_slice::<Value>(&raw_bytes) else {
        return;
    };
    let Some(config) = raw
        .as_object_mut()
        .map(|obj| obj.entry("config").or_insert_with(|| json!({})))
    else {
        return;
    };
    if config.get("defaultModelSelection") == Some(&wanted) {
        return;
    }
    config["defaultModelSelection"] = wanted;
    let tmp = personal_file.with_extension("json.tmp-gateforge");
    if let Ok(body) = serde_json::to_string_pretty(&raw) {
        if std::fs::write(&tmp, body + "\n").is_ok() {
            let _ = std::fs::rename(&tmp, personal_file);
        }
    }
}

fn resolve_upstream_model_id(model: &str) -> String {
    if model.eq_ignore_ascii_case("glm-5.3") {
        "GLM-5.3".to_string()
    } else {
        "GLM-5.3-Flash".to_string()
    }
}

fn prepare_start_plan_worker_env(model: &str) -> SpawnTarget {
    let chosen = resolve_upstream_model_id(model);
    let personal_file = home_dir()
        .join(".zcode")
        .join("v2")
        .join("provider_config.json");
    ensure_start_plan_default_selection(&personal_file, &chosen);
    let selection = json!({
        "providerId": agent_provider_id(),
        "modelId": chosen,
    });
    let mut target = resolve_app_server_command();
    if let Some(builtin) = discover_builtin_provider_config() {
        target.env.insert(
            "ZCODE_BUILTIN_PROVIDER_CONFIG_FILE".into(),
            builtin.to_string_lossy().into_owned(),
        );
    }
    if personal_file.exists() {
        if let Some(bridge_copy) = build_bridge_personal_config(&personal_file, &selection) {
            target.env.insert(
                "ZCODE_PERSONAL_PROVIDER_CONFIG_FILE".into(),
                bridge_copy.to_string_lossy().into_owned(),
            );
        }
    }
    target
}

// ---------------------------------------------------------------------------
// Shared local profile auth (nothing is stored — the profile owns it)
// ---------------------------------------------------------------------------

// The shared zcode JWT lives encrypted in ~/.zcode/v2/credentials.json with
// the same AES-256-GCM scheme the ZCode app uses; the key derives from the
// local machine identity unless ZCODE_CREDENTIAL_SECRET is set.
pub fn read_local_zcode_jwt() -> String {
    let file = home_dir().join(".zcode").join("v2").join("credentials.json");
    let Ok(bytes) = std::fs::read(&file) else {
        return String::new();
    };
    let Ok(creds) = serde_json::from_slice::<Value>(&bytes) else {
        return String::new();
    };
    let Some(encrypted) = creds.get("zcodejwttoken").and_then(|v| v.as_str()) else {
        return String::new();
    };
    decrypt_credential_value(encrypted)
}

pub fn decrypt_credential_value(value: &str) -> String {
    let Some(rest) = value.strip_prefix("enc:v1:") else {
        return value.to_string();
    };
    let mut parts = rest.split('.');
    let (Some(iv_raw), Some(tag_raw), Some(cipher_raw)) =
        (parts.next(), parts.next(), parts.next())
    else {
        return String::new();
    };
    let secret = std::env::var("ZCODE_CREDENTIAL_SECRET").unwrap_or_else(|_| {
        let platform = if cfg!(windows) {
            "win32"
        } else if cfg!(target_os = "macos") {
            "darwin"
        } else {
            "linux"
        };
        let username = if cfg!(windows) {
            std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into())
        } else {
            std::env::var("USER").unwrap_or_else(|_| "unknown".into())
        };
        format!(
            "zcode-credential-fallback:{platform}:{}:{username}",
            home_dir().to_string_lossy()
        )
    });
    let key: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
    let Ok(cipher) = Aes256Gcm::new_from_slice(&key) else {
        return String::new();
    };
    let (Ok(iv), Ok(tag), Ok(ciphertext)) = (
        B64URL.decode(iv_raw),
        B64URL.decode(tag_raw),
        B64URL.decode(cipher_raw),
    ) else {
        return String::new();
    };
    let mut sealed = ciphertext;
    sealed.extend_from_slice(&tag);
    let nonce = Nonce::from_slice(&iv);
    cipher
        .decrypt(nonce, sealed.as_ref())
        .map(|plain| String::from_utf8_lossy(&plain).into_owned())
        .unwrap_or_default() // wrong machine/key — the agent surfaces the auth failure
}

// The device id lives in the shared ZCode profile next to the credentials.
pub fn read_zcode_device_mid() -> String {
    let file = home_dir()
        .join(".zcode")
        .join("v2")
        .join("telemetry-state.json");
    let Ok(bytes) = std::fs::read(&file) else {
        return String::new();
    };
    serde_json::from_slice::<Value>(&bytes)
        .ok()
        .and_then(|state| {
            state
                .get("deviceMid")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Prompt flattening + response builders
// ---------------------------------------------------------------------------

// The bridge is prompt-shaped: flatten the (OpenAI-format) request body into a
// single agent prompt. Tool-call round-trips through the router are out of
// scope — the agent runs with ZCode's own tool set inside the session.
pub fn flatten_openai_prompt(body: &Value) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = body.get("system") {
        let text = normalize_system(system);
        if !text.trim().is_empty() {
            system_parts.push(text.trim().to_string());
        }
    }
    if let Some(messages) = body.get("messages").and_then(|m| m.as_array()) {
        for message in messages {
            let role = message
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("user");
            let text = normalize_content(message.get("content").unwrap_or(&Value::Null));
            if text.is_empty() {
                continue;
            }
            if role == "system" {
                system_parts.push(text);
            } else {
                let label = if role == "assistant" { "Assistant" } else { "User" };
                parts.push(format!("{label}: {text}"));
            }
        }
    }
    if !system_parts.is_empty() {
        parts.insert(0, format!("# System\n{}", system_parts.join("\n")));
    }
    if parts.is_empty() {
        return "Hello".to_string();
    }
    parts.join("\n\n")
}

fn normalize_system(system: &Value) -> String {
    match system {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn normalize_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.trim().to_string(),
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|block| match block.get("type").and_then(|t| t.as_str()) {
                Some("text") => block.get("text").and_then(|t| t.as_str()).map(String::from),
                Some("tool_result") => {
                    let inner = normalize_content(block.get("content").unwrap_or(&Value::Null));
                    Some(format!(
                        "[tool_result] {}",
                        inner.chars().take(2000).collect::<String>()
                    ))
                }
                Some("tool_use") => Some(format!(
                    "[tool_use {}]",
                    block.get("name").and_then(|n| n.as_str()).unwrap_or("")
                )),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
        _ => String::new(),
    }
}

fn approx_tokens(text: &str) -> u64 {
    ((text.chars().count() as u64) / 4).max(1)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn new_message_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("msg_gf_{nanos:x}")
}

pub fn openai_json_response(model: &str, text: &str) -> Value {
    let tokens = approx_tokens(text);
    json!({
        "id": new_message_id(),
        "object": "chat.completion",
        "created": now_unix(),
        "model": model,
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
        "usage": {
            "prompt_tokens": 0,
            "completion_tokens": tokens,
            "total_tokens": tokens,
        },
    })
}

pub fn openai_sse_body(model: &str, text: &str) -> String {
    let id = new_message_id();
    let created = now_unix();
    let chunk = |delta: Value, finish: Option<&str>| -> String {
        let payload = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": created,
            "model": model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": finish,
            }],
        });
        format!(
            "data: {}\n\n",
            serde_json::to_string(&payload).unwrap_or_default()
        )
    };
    format!(
        "{}{}",
        chunk(json!({ "role": "assistant", "content": text }), None),
        chunk(json!({}), Some("stop")),
    ) + "data: [DONE]\n\n"
}

// Bridge failures are otherwise invisible; append a compact breadcrumb to the
// data dir so the failing stage can be diagnosed from disk.
fn log_bridge(stage: &str, message: &str, meta: Value) {
    let mut record = json!({
        "ts": now_unix(),
        "stage": stage,
        "message": message,
    });
    if let (Some(object), Some(meta)) = (record.as_object_mut(), meta.as_object()) {
        for (key, value) in meta {
            object.insert(key.clone(), value.clone());
        }
    }
    let line = serde_json::to_string(&record).unwrap_or_default();
    let dir = store::default_data_dir().join("logs");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("zcode-bridge.log"))
        .and_then(|mut file| {
            std::io::Write::write_all(&mut file, format!("{line}\n").as_bytes())
        });
}

// ---------------------------------------------------------------------------
// Public dispatch entry: one JSON line to the node bridge client
// ---------------------------------------------------------------------------

const BRIDGE_CLIENT: &str = include_str!("../assets/zcode-bridge-client.cjs");

fn materialize_bridge_client() -> Result<PathBuf, String> {
    let dir = store::default_data_dir().join("zcode-bridge");
    std::fs::create_dir_all(&dir).map_err(|error| error.to_string())?;
    let path = dir.join("zcode-bridge-client.cjs");
    std::fs::write(&path, BRIDGE_CLIENT).map_err(|error| error.to_string())?;
    Ok(path)
}

// Run one turn through the local ZCode runtime. Server dist first (self-managed
// account providers), desktop CLI app-server as fallback (Start Plan pinned).
pub async fn dispatch(model_id: &str, openai_body: &Value) -> Result<String, String> {
    let (mode, target) = if let Some(server) = resolve_server_dist_command() {
        ("server", server)
    } else {
        ("appserver", prepare_start_plan_worker_env(model_id))
    };
    let client_script = materialize_bridge_client()?;
    let request = json!({
        "mode": mode,
        "command": target.command,
        "args": target.args,
        "env": target.env,
        "jwt": read_local_zcode_jwt(),
        "model": model_id,
        "providerId": agent_provider_id(),
        "requireStartPlan": env_flag("ZCODE_REQUIRE_START_PLAN", true),
        "allowTools": env_flag("ZCODE_ALLOW_TOOLS", false),
        "workspacePath": workspace_path(),
        "prompt": flatten_openai_prompt(openai_body),
        "timeouts": {
            "startupMs": startup_timeout_ms(),
            "rpcMs": rpc_timeout_ms(),
            "turnMs": turn_timeout_ms(),
            "pollMs": poll_interval_ms(),
        },
    });
    log_bridge(
        "dispatch",
        "spawning node bridge client",
        json!({
            "mode": mode,
            "command": target.command,
            "args": target.args.join(" "),
            "model": model_id,
        }),
    );

    let spawn_target = SpawnTarget {
        command: resolve_node(),
        args: vec![client_script.to_string_lossy().into_owned()],
        env: BTreeMap::new(),
    };
    let request_line = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
    // Overall window: startup + turn + generous RPC slack.
    let budget = Duration::from_millis(
        startup_timeout_ms() + turn_timeout_ms() + rpc_timeout_ms() * 2,
    );
    let result = tokio::time::timeout(budget, run_bridge_exchange(&spawn_target, &request_line))
        .await
        .map_err(|_| format!("ZCode bridge timed out after {}s", budget.as_secs()))?;
    match result {
        Ok(reply) if reply.get("ok") == Some(&json!(true)) => Ok(reply
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or_default()
            .to_string()),
        Ok(reply) => {
            let error = reply
                .get("error")
                .and_then(|e| e.as_str())
                .unwrap_or("bridge returned no error detail")
                .to_string();
            log_bridge("dispatch", &error, json!({ "model": model_id }));
            Err(error)
        }
        Err(error) => {
            log_bridge("dispatch", &error, json!({ "model": model_id }));
            Err(error)
        }
    }
}

async fn run_bridge_exchange(target: &SpawnTarget, request_line: &[u8]) -> Result<Value, String> {
    let mut process = spawn_child(target).await?;
    let outcome = async {
        process.write_line(request_line).await?;
        let line = process.read_line().await?;
        serde_json::from_str::<Value>(&line)
            .map_err(|error| format!("invalid bridge result: {error} — {line}"))
    }
    .await;
    process.kill().await;
    outcome
}

// HTTP status for a bridge failure, mirroring the 9router executor mapping.
pub fn bridge_error_status(message: &str) -> u16 {
    let lowered = message.to_lowercase();
    if lowered.contains("aborted") {
        499
    } else if lowered.contains("not installed")
        || lowered.contains("enoent")
        || lowered.contains("handshake timed out")
        || lowered.contains("failed to spawn")
    {
        503
    } else {
        502
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vql_roundtrip() {
        for value in [0u64, 1, 127, 128, 16383, 16384, u64::from(u32::MAX), 1 << 40] {
            let mut bytes = Vec::new();
            encode_vql(value, &mut bytes);
            let mut offset = 0usize;
            let decoded = decode_vql(&bytes, &mut offset).expect("decode");
            assert_eq!(decoded, value);
            assert_eq!(offset, bytes.len());
        }
    }

    #[test]
    fn value_codec_roundtrip() {
        let value = json!({
            "workspacePath": "D:/Project/gateforge",
            "count": 42,
            "nested": { "mode": "build", "enabled": true },
            "list": [1, "two", null, 3],
            "float": 1.5,
        });
        let mut bytes = Vec::new();
        encode_zcode_value(&value, &mut bytes);
        let mut offset = 0usize;
        let decoded = decode_zcode_value(&bytes, &mut offset).expect("decode");
        assert_eq!(decoded, value);
        assert_eq!(offset, bytes.len());
    }

    #[test]
    fn rpc_frame_layout() {
        let frame = encode_rpc_call(7, "zcode-agent", "initialize", &json!([{"a": 1}]));
        assert_eq!(frame[0], REGULAR_MESSAGE);
        let length = u32::from_be_bytes([frame[9], frame[10], frame[11], frame[12]]);
        assert_eq!(frame.len(), HEADER_SIZE + length as usize);
        let body = &frame[HEADER_SIZE..];
        let mut offset = 0usize;
        let header = decode_zcode_value(body, &mut offset).expect("header");
        assert_eq!(header, json!([100, 7, "zcode-agent", "initialize"]));
        let args = decode_zcode_value(body, &mut offset).expect("args");
        assert_eq!(args, json!([{"a": 1}]));
        assert_eq!(offset, body.len());
    }

    #[test]
    fn flatten_prompt_shapes() {
        let body = json!({
            "messages": [
                { "role": "system", "content": "You are terse." },
                { "role": "user", "content": [
                    { "type": "text", "text": "hello" },
                    { "type": "tool_use", "name": "read_file" },
                ]},
                { "role": "assistant", "content": "earlier answer" },
                { "role": "user", "content": "again" },
            ]
        });
        let prompt = flatten_openai_prompt(&body);
        assert!(prompt.starts_with("# System\nYou are terse."));
        assert!(prompt.contains("User: hello\n[tool_use read_file]"));
        assert!(prompt.contains("Assistant: earlier answer"));
        assert!(prompt.ends_with("User: again"));
    }

    #[test]
    fn flatten_prompt_empty_falls_back_to_hello() {
        assert_eq!(flatten_openai_prompt(&json!({ "messages": [] })), "Hello");
    }

    #[test]
    fn upstream_model_resolution() {
        assert_eq!(resolve_upstream_model_id("GLM-5.3"), "GLM-5.3");
        assert_eq!(resolve_upstream_model_id("glm-5.3-flash"), "GLM-5.3-Flash");
    }

    #[test]
    fn credential_decrypt_roundtrip() {
        // Encrypt with the same AES-256-GCM scheme the ZCode app uses, then
        // decrypt through the bridge path with a fixed test secret.
        let secret = "test-secret";
        let key: [u8; 32] = Sha256::digest(secret.as_bytes()).into();
        let cipher = Aes256Gcm::new_from_slice(&key).expect("key");
        let iv: [u8; 12] = [7u8; 12];
        let sealed = cipher
            .encrypt(Nonce::from_slice(&iv), b"bridge-jwt-payload".as_ref())
            .expect("encrypt");
        let (ciphertext, tag) = sealed.split_at(sealed.len() - 16);
        let value = format!(
            "enc:v1:{}.{}.{}",
            B64URL.encode(iv),
            B64URL.encode(tag),
            B64URL.encode(ciphertext)
        );
        std::env::set_var("ZCODE_CREDENTIAL_SECRET", secret);
        assert_eq!(decrypt_credential_value(&value), "bridge-jwt-payload");
        assert_eq!(decrypt_credential_value("plain-value"), "plain-value");
        std::env::remove_var("ZCODE_CREDENTIAL_SECRET");
    }

    #[test]
    fn bridge_error_status_mapping() {
        assert_eq!(bridge_error_status("failed to spawn node ENOENT"), 503);
        assert_eq!(bridge_error_status("handshake timed out"), 503);
        assert_eq!(bridge_error_status("aborted"), 499);
        assert_eq!(bridge_error_status("ZCode turn timed out after 300s"), 502);
    }

    #[test]
    fn response_builders() {
        let json = openai_json_response("zcode/glm-5.3-flash", "hi");
        assert_eq!(
            json.pointer("/choices/0/message/content"),
            Some(&json!("hi"))
        );
        assert_eq!(json.pointer("/choices/0/finish_reason"), Some(&json!("stop")));
        let sse = openai_sse_body("zcode/glm-5.3-flash", "hi");
        assert!(sse.starts_with("data: {"));
        assert!(sse.contains("\"finish_reason\":\"stop\""));
        assert!(sse.ends_with("data: [DONE]\n\n"));
    }
}
