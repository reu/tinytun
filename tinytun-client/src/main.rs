use std::{error::Error, sync::Arc};

use clap::Parser;
use hyper::{server::conn, service::service_fn, upgrade, Body, Client, Method, Request, Uri};
use hyper_tls::HttpsConnector;

use crate::proxy::ReverseProxy;

mod proxy;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Service port
    #[arg(short, long)]
    port: u16,

    /// Connection management port
    #[arg(short, long, default_value_t = Uri::from_static("http://local.tinytun.com:5554"))]
    server_url: Uri,

    /// Proxy URL
    #[arg(long, default_value_t = Uri::from_static("http://local.tinytun.com:5555"))]
    proxy_url: Uri,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();

    let target = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{}", args.port))
        .path_and_query("/")
        .build()?;

    let res = Client::builder()
        .build(HttpsConnector::new())
        .request(
            Request::builder()
                .uri(args.server_url)
                .method(Method::CONNECT)
                .body(Body::empty())?,
        )
        .await?;

    let conn_id = res
        .headers()
        .get("x-tinytun-connection-id")
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

    let proxy = Arc::new(ReverseProxy::new());

    conn::Http::new()
        .serve_connection(
            upgrade::on(res).await?,
            service_fn(|req| {
                let target = target.clone();
                let proxy = proxy.clone();
                async move { proxy.proxy(&target, req).await }
            }),
        )
        .await?;

    Ok(())
}
