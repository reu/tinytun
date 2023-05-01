use std::error::Error;

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

    /// Connection management server URL
    #[arg(long, default_value_t = default_api_url())]
    server_url: Uri,

    /// Proxy URL
    #[arg(long, default_value_t = default_proxy_url())]
    proxy_url: Uri,

    /// Subdomain to use
    #[arg(short, long)]
    subdomain: Option<String>,

    /// Maximum number of concurrent connections allowed
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

    let mut tun = Tunnel::builder()
        .server_url(args.server_url)
        .base_url(args.proxy_url)
        .subdomain(args.subdomain)
        .max_concurrent_streams(args.concurrency)
        .connect()
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
