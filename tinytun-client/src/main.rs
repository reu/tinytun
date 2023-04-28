use std::{error::Error, process::exit};

use bytes::{Bytes, BytesMut};
use clap::Parser;
use hyper::{body, upgrade, Body, Client, Method, Request, Response, Uri};
use hyper_rustls::HttpsConnectorBuilder;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    try_join,
};

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

    let res = Client::builder()
        .build(
            HttpsConnectorBuilder::new()
                .with_native_roots()
                .https_or_http()
                .enable_http1()
                .build(),
        )
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

    let remote = upgrade::on(res).await?;
    let mut connection = h2::server::Builder::new()
        .max_concurrent_streams(args.concurrency)
        .handshake(remote)
        .await?;
    while let Some(result) = connection.accept().await {
        let (remote_req, mut remote_respond) = result?;

        tokio::spawn(async move {
            let mut remote_body = remote_req.into_body();
            let local_stream = TcpStream::connect(format!("localhost:{}", args.port)).await?;
            let (mut reader, mut writer) = local_stream.into_split();

            let read = async move {
                let mut flow_control = remote_body.flow_control().clone();
                while let Some(data) = remote_body.data().await {
                    let data = data?;
                    if data.is_empty() {
                        break;
                    }
                    flow_control.release_capacity(data.len())?;
                    writer.write_all(&data).await?;
                }
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            };

            let write = async move {
                let mut send_stream = remote_respond.send_response(Response::new(()), false)?;
                let mut buf = vec![0; 8 * 1024];
                loop {
                    match reader.read(&mut buf).await? {
                        0 => {
                            send_stream.send_data(Bytes::default(), true)?;
                            break;
                        }
                        n => {
                            let buf = BytesMut::from(&buf[0..n]);
                            send_stream.send_data(buf.into(), false)?;
                        }
                    }
                }
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            };

            let (write, read) = try_join!(tokio::spawn(write), tokio::spawn(read))?;

            match (write, read) {
                (Err(err), _) | (_, Err(err)) => {
                    eprintln!("Error: {err}");
                }
                _ => {}
            }

            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
    }

    Ok(())
}
