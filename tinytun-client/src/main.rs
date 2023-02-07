use std::{error::Error, process::exit};

use clap::Parser;
use hyper::{body, upgrade, Body, Client, Method, Request, Uri};
use hyper_tls::HttpsConnector;
use tokio::{io, net::TcpStream};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Service port
    #[arg(short, long)]
    port: u16,

    /// Connection management port
    #[arg(long, default_value_t = Uri::from_static("http://local.tinytun.com:5554"))]
    server_url: Uri,

    /// Proxy URL
    #[arg(long, default_value_t = Uri::from_static("http://local.tinytun.com:5555"))]
    proxy_url: Uri,

    /// Subdomain to use
    #[arg(short, long)]
    subdomain: Option<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();

    let res = Client::builder()
        .build(HttpsConnector::new())
        .request({
            let req = Request::builder()
                .uri(args.server_url)
                .method(Method::CONNECT);

            match args.subdomain {
                Some(subdomain) if !subdomain.trim().is_empty() => req
                    .header("x-tinytun-subdomain", subdomain)
                    .body(Body::empty())?,
                _ => req.body(Body::empty())?,
            }
        })
        .await?;

    if res.status().is_client_error() {
        let message = body::to_bytes(res.into_body())
            .await
            .ok()
            .and_then(|body| {
                std::str::from_utf8(&body)
                    .map(|message| message.to_owned())
                    .ok()
            })
            .unwrap_or_else(|| "Couldn't retrive connection".to_string());

        println!("Error: {message}");

        exit(1)
    }

    let conn_id = res
        .headers()
        .get("x-tinytun-subdomain")
        .ok_or("Server didn't provide a connection id")?
        .to_str()?;

    let proxy_url = Uri::from_parts({
        let mut proxy_url = args.proxy_url.into_parts();
        proxy_url.authority = {
            let authority = format!("{conn_id}.{}", proxy_url.authority.unwrap());
            Some(authority.parse()?)
        };
        proxy_url
    })?;

    println!("Forwarding via: {proxy_url}");

    let mut remote = upgrade::on(res).await?;
    let mut local = TcpStream::connect(format!("localhost:{}", args.port)).await?;

    io::copy_bidirectional(&mut remote, &mut local).await?;

    Ok(())
}
