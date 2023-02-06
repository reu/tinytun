use std::{error::Error, process::exit, sync::Arc};

use clap::Parser;
use hyper::{body, server::conn, service::service_fn, upgrade, Body, Client, Method, Request, Uri};
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

    let target = Uri::builder()
        .scheme("http")
        .authority(format!("localhost:{}", args.port))
        .path_and_query("/")
        .build()?;

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
