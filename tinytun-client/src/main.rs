use std::{env, error::Error};

use clap::Parser;
use http::Uri;
use tinytun::Tunnel;
use tokio::{io, net::TcpStream};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Local web service port
    port: u16,

    /// Subdomain to use
    #[arg(short, long)]
    subdomain: Option<String>,

    /// Maximum number of concurrent connections allowed on the local service
    #[arg(short, long, default_value_t = 100)]
    concurrency: u32,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();

    let server_url = env::var("TINYTUN_SERVER_URL")
        .ok()
        .and_then(|url| url.parse::<Uri>().ok())
        .unwrap_or_else(|| {
            let default = option_env!("TINYTUN_DEFAULT_SERVER_URL");
            Uri::from_static(default.unwrap_or("http://local.tinytun.com:5554"))
        });

    let mut tun = Tunnel::builder()
        .server_url(server_url)
        .subdomain(args.subdomain)
        .max_concurrent_streams(args.concurrency)
        .listen()
        .await?;

    println!("Forwarding via: {}", tun.proxy_url());

    while let Some(mut remote_stream) = tun.accept().await {
        tokio::spawn(async move {
            let local_address = format!("localhost:{}", args.port);
            let mut local_stream = match TcpStream::connect(&local_address).await {
                Ok(stream) => stream,
                Err(err) => {
                    eprintln!("Failed to connect to {local_address}");
                    return Err(err);
                }
            };
            io::copy_bidirectional(&mut remote_stream, &mut local_stream).await?;
            io::Result::Ok(())
        });
    }

    Ok(())
}
