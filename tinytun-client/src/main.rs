use std::{env, error::Error};

use clap::Parser;
use http::Uri;
use tinytun::Tunnel;
use tokio::{io, net::TcpStream};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Service port
    #[arg(short, long)]
    port: u16,

    /// Subdomain to use
    #[arg(short, long)]
    subdomain: Option<String>,

    /// Maximum number of concurrent connections allowed on the local service
    #[arg(short, long, default_value_t = 100)]
    concurrency: u32,
}

fn default_api_url() -> Uri {
    let default = option_env!("TINYTUN_DEFAULT_SERVER_URL");
    Uri::from_static(default.unwrap_or("http://local.tinytun.com:5554"))
}

fn default_proxy_url() -> Uri {
    let default = option_env!("TINYTUN_DEFAULT_PROXY_URL");
    Uri::from_static(default.unwrap_or("http://local.tinytun.com:5555"))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();

    let server_url = env::var("TINYTUN_SERVER_URL")
        .ok()
        .and_then(|url| url.parse::<Uri>().ok())
        .unwrap_or(default_api_url());

    let proxy_url = env::var("TINYTUN_PROXY_URL")
        .ok()
        .and_then(|url| url.parse::<Uri>().ok())
        .unwrap_or(default_proxy_url());

    let mut tun = Tunnel::builder()
        .server_url(server_url)
        .base_url(proxy_url)
        .subdomain(args.subdomain)
        .max_concurrent_streams(args.concurrency)
        .listen()
        .await?;

    println!("Forwarding via: {}", tun.proxy_url());

    while let Some(mut remote_stream) = tun.accept().await {
        let mut local_stream = TcpStream::connect(format!("localhost:{}", args.port)).await?;
        tokio::spawn(async move {
            io::copy_bidirectional(&mut remote_stream, &mut local_stream)
                .await
                .ok();
        });
    }

    Ok(())
}
