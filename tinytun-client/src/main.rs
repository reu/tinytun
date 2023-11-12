use std::{env, error::Error, time::Duration};

use clap::{Parser, ValueEnum};
use http::Uri;
use tinytun::Tunnel;
use tokio::{io, net::TcpStream, select, signal, time::sleep};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
enum TunnelType {
    Tcp,
    Http,
}

impl From<TunnelType> for tinytun::TunnelType {
    fn from(value: TunnelType) -> Self {
        match value {
            TunnelType::Tcp => tinytun::TunnelType::Tcp,
            TunnelType::Http => tinytun::TunnelType::Http,
        }
    }
}

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Type
    tunnel_type: TunnelType,

    /// Local web service port
    local_port: u16,

    /// Subdomain to use
    #[arg(short, long)]
    subdomain: Option<String>,

    /// Remote port to use (must be in the 20000-60000 range)
    #[arg(short, long)]
    port: Option<u16>,

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

    let interrupt = signal::ctrl_c();

    let proxy = async move {
        let mut retries = 0;

        loop {
            let mut tun = match Tunnel::builder()
                .server_url(server_url.clone())
                .subdomain(args.subdomain.clone())
                .port(args.port)
                .max_concurrent_streams(args.concurrency)
                .tunnel_type(args.tunnel_type)
                .listen()
                .await
            {
                Ok(tun) => tun,
                Err(_err) => {
                    sleep(Duration::from_secs(2)).await;

                    retries += 1;
                    if retries > 10 {
                        break;
                    }

                    continue;
                }
            };

            if retries == 0 {
                println!("Forwarding via: {}", tun.proxy_url());
            }

            while let Some(mut remote_stream) = tun.accept().await {
                tokio::spawn(async move {
                    let local_address = format!("localhost:{}", args.local_port);
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

            sleep(Duration::from_secs(2)).await;

            retries += 1;
            if retries > 20 {
                break;
            }
        }

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    select! {
        _ = interrupt => {
            eprintln!("Exiting...");
        },
        _ = proxy => {
            println!("Server is down");
        },
    };

    Ok(())
}
