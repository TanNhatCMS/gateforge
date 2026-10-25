// GateForge Tauri shell: starts the gateway on a dedicated thread, polls
// until the port is open, then opens the webview pointed at the gateway URL.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gateway::default_port;
use std::net::TcpStream;

fn main() {
    let port = default_port();
    // Start the gateway on a dedicated thread.
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("gateway tokio runtime");
        if let Err(error) = runtime.block_on(gateway::start_server(port)) {
            eprintln!("[gateforge] server failed: {error}");
        }
    });

    // Poll until the gateway port opens (max 10s).
    let addr = format!("127.0.0.1:{port}");
    let mut bound = false;
    for _ in 0..50 {
        if TcpStream::connect(&addr).is_ok() {
            bound = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    if bound {
        eprintln!("[gateforge] gateway ready on {addr}");
    } else {
        eprintln!("[gateforge] gateway not ready within 10s — opening window anyway");
    }

    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running GateForge");
}
