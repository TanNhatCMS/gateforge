// Auth + key generation — port of 9router src/shared/utils/machineId.js,
// apiKey.js and src/lib/auth/dashboardSession.js, scoped to what GateForge
// needs: machine id, API key minting/validation, dashboard sessions.
use std::path::{Path, PathBuf};

pub const API_KEY_SECRET: &str = "endpoint-proxy-api-key-secret";
const CLI_TOKEN_SALT: &str = "9r-cli-auth";

fn data_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("DATA_DIR") {
        if !dir.trim().is_empty() {
            return PathBuf::from(dir);
        }
    }
    if cfg!(windows) {
        let appdata = std::env::var("APPDATA").unwrap_or_else(|_| {
            home_dir().join("AppData").join("Roaming").to_string_lossy().into_owned()
        });
        return Path::new(&appdata).join("9router");
    }
    home_dir().join(".9router")
}

fn home_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

/// Raw machine id — persisted once (node-machine-id equivalent).
pub fn machine_id() -> String {
    let file = data_dir().join("machine-id");
    if let Ok(id) = std::fs::read_to_string(&file) {
        let id = id.trim().to_string();
        if !id.is_empty() {
            return id;
        }
    }
    let id = uuid_v4();
    std::fs::create_dir_all(data_dir()).ok();
    std::fs::write(&file, &id).ok();
    id
}

fn uuid_v4() -> String {
    let bytes = rand_bytes();
    let mut b = bytes;
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
    )
}

// xorshift64* seeded from the OS clock — fine for key ids, not for secrets.
fn rand_bytes() -> [u8; 16] {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        if x == 0 {
            x = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E3779B97F4A7C15)
                | 1;
        }
        let mut out = [0u8; 16];
        for chunk in out.chunks_mut(8) {
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            s.set(x);
            let val = x.wrapping_mul(0x2545F4914F6CDD1D);
            for (i, b) in chunk.iter_mut().enumerate() {
                *b = ((val >> (i * 8)) & 0xff) as u8;
            }
        }
        out
    })
}

fn hex32(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── HMAC-SHA256 via the sha2 crate (hand-rolled impl had parity issues) ──
use sha2::{Digest, Sha256};

fn hmac_sha256(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        let mut hasher = Sha256::new();
        hasher.update(key);
        k[..32].copy_from_slice(&hasher.finalize());
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner_input = ipad;
    inner_input.extend_from_slice(data);
    let mut hasher = Sha256::new();
    hasher.update(&inner_input);
    let inner = hasher.finalize();
    let mut outer_input = opad;
    outer_input.extend_from_slice(&inner);
    let mut hasher = Sha256::new();
    hasher.update(&outer_input);
    hasher.finalize().try_into().unwrap()
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher.finalize().try_into().unwrap()
}

fn hmac_sha256_truncated(key: &str, data: &str, chars: usize) -> String {
    let mac = hmac_sha256(key.as_bytes(), data.as_bytes());
    hex32(&mac)[..chars].to_string()
}

/// 16-char machine token (dashboardGuard parity: machineId + salt + cli secret).
pub fn consistent_machine_id(salt: &str) -> String {
    let raw = machine_id();
    let secret = cli_secret();
    let extra = if salt == CLI_TOKEN_SALT { secret.as_str() } else { "" };
    let digest = sha256(format!("{raw}{salt}{extra}").as_bytes());
    hex32(&digest)[..16].to_string()
}

fn cli_secret() -> String {
    let file = data_dir().join("auth").join("cli-secret");
    if let Ok(s) = std::fs::read_to_string(&file) {
        let s = s.trim().to_string();
        if !s.is_empty() {
            return s;
        }
    }
    let s = hex32(&rand_bytes());
    std::fs::create_dir_all(file.parent().unwrap()).ok();
    std::fs::write(&file, &s).ok();
    s
}

/// Generate the GateForge API key: sk-{machineId}-{keyId}-{crc8}.
pub struct GeneratedKey {
    pub key: String,
    pub key_id: String,
}

pub fn generate_api_key(machine_id: &str) -> GeneratedKey {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = rand_bytes();
    let mut key_id = String::new();
    for b in bytes.iter().take(6) {
        key_id.push(CHARS[(b % CHARS.len() as u8) as usize] as char);
    }
    let mac = hmac_sha256(
        API_KEY_SECRET.as_bytes(),
        format!("{machine_id}{key_id}").as_bytes(),
    );
    let crc = hex32(&mac)[..8].to_string();
    GeneratedKey {
        key: format!("sk-{machine_id}-{key_id}-{crc}"),
        key_id,
    }
}

/// Validate a pasted API key against the machine id embedded in it.
pub fn validate_api_key_shape(key: &str, machine_id: &str) -> bool {
    let parts: Vec<&str> = key.split('-').collect();
    parts.len() == 4
        && parts[0] == "sk"
        && parts[1] == machine_id
        && parts[2].len() == 6
        && parts[3].len() == 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_node_reference() {
        // Precomputed with node: HMAC-SHA256("endpoint-proxy-api-key-secret", "abc123")[:8]
        assert_eq!(
            hmac_sha256_truncated("endpoint-proxy-api-key-secret", "abc123", 8),
            "e3ba06e4"
        );
    }

    #[test]
    fn generated_key_shape_and_validation() {
        let machine = "aaaaaaaaaaaaaaaa";
        let generated = generate_api_key(machine);
        assert!(generated.key.starts_with(&format!("sk-{machine}-")));
        assert_eq!(generated.key_id.len(), 6);
        assert!(validate_api_key_shape(&generated.key, machine));
        assert!(!validate_api_key_shape("sk-wrong-key-format", machine));
    }
}
