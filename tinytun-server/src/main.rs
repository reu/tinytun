use std::{env, error::Error, sync::Arc};

use tokio::{net::TcpListener, try_join};

use tracing::metadata::LevelFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use tinytun_server::tunnel::Tunnels;
use tinytun_server::{start_api, start_http_proxy, start_metadata_api, start_tcp_proxy};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .from_env_lossy(),
        )
        .init();

    let metadata_port = env::var("METADATA_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5553);

    let conn_port = env::var("CONNECTION_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5554);

    let proxy_port = env::var("PROXY_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5555);

    let tcp_proxy_port = env::var("TCP_PROXY_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5556);

    let base_domain = env::var("BASE_DOMAIN").ok().unwrap_or_else(|| {
        option_env!("DEFAULT_BASE_DOMAIN")
            .unwrap_or("local.tinytun.com")
            .to_string()
    });
    let base_domain = Arc::new(base_domain);

    let tuns = Arc::new(Tunnels::new());

    let conn_listener = TcpListener::bind(("0.0.0.0", conn_port)).await?;
    let metadata_listener = TcpListener::bind(("0.0.0.0", metadata_port)).await?;
    let proxy_listener = TcpListener::bind(("0.0.0.0", proxy_port)).await?;
    let tcp_proxy_listener = TcpListener::bind(("0.0.0.0", tcp_proxy_port)).await?;

    let api = start_api(tuns.clone(), base_domain.clone(), conn_listener);
    let metadata_api = start_metadata_api(tuns.clone(), metadata_listener);
    let http_proxy = start_http_proxy(tuns.clone(), proxy_listener);
    let tcp_proxy = start_tcp_proxy(tuns.clone(), tcp_proxy_listener);

    let _ = try_join!(
        tokio::spawn(api),
        tokio::spawn(http_proxy),
        tokio::spawn(tcp_proxy),
        tokio::spawn(metadata_api),
    )?;

    Ok(())
}
