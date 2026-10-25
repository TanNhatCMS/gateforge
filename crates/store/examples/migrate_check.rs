// One-shot migration check: open a fresh DATA_DIR without GATEFORGE_NO_MIGRATE
// so the first-boot 9router copy runs against the real database.
fn main() {
    let dir = std::env::temp_dir().join("migrate-check");
    let _ = std::fs::remove_dir_all(&dir);
    let store = store::Store::open(&dir).expect("open");
    let conns = store.list_connections(None).expect("conns");
    println!("migrated connections: {}", conns.len());
    for c in conns.iter().take(6) {
        println!("  - {} ({}, active={})", c.provider, c.auth_type, c.is_active);
    }
    let keys = store.list_api_keys().expect("keys");
    println!("api keys: {}", keys.len());
    println!("settings present: {}", store.get_settings().is_some());
}
