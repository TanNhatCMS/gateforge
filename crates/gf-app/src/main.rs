// GateForge Tauri shell: starts the gateway server on a dedicated thread
// (own tokio runtime — immune to shell/runtime ordering), then opens the
// webview pointed at the local gateway URL.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use gf_gateway::default_port;

fn main() {
    let port = default_port();
    std::thread::spawn(move || {
        let debug = |line: &str| {
            std::fs::write(
                "D:\\Project\\gateforge\\gf-debug.log",
                format!("{line}\n"),
            )
            .ok(); // last-line marker (temporary diagnostics)
        };
        debug("thread started");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("gateway tokio runtime");
        debug("runtime built");
        runtime.block_on(async move {
            debug("binding...");
            let router = gf_gateway::build_router();
            let listener = match tokio::net::TcpListener::bind(("0.0.0.0", port)).await {
                Ok(listener) => {
                    debug(&format!("bound on {port}"));
                    listener
                }
                Err(error) => {
                    debug(&format!("bind failed: {error}"));
                    return;
                }
            };
            debug("serving");
            if let Err(error) = axum::serve(listener, router).await {
                debug(&format!("serve failed: {error}"));
            }
        });
    });
    tauri::Builder::default()
        .run(tauri::generate_context!())
        .expect("error while running GateForge");
}
