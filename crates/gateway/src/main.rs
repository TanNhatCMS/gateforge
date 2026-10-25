// Thin binary wrapper: `gateforge` starts the gateway server standalone.
#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gf=info,tower_http=info".into()),
        )
        .init();

    let port = gateway::default_port();
    if let Err(error) = gateway::start_server(port).await {
        tracing::error!("gateway failed: {error}");
        std::process::exit(1);
    }
}
