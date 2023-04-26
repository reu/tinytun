use std::{
    error::Error,
    future::Future,
    pin::Pin,
    process::exit,
    task::{self, Poll},
};

use bytes::{Bytes, BytesMut};
use clap::Parser;
use hyper::{
    body::{self, HttpBody},
    header, upgrade, Body, Client, HeaderMap, Method, Request, Response, Uri, Version,
};
use hyper_tls::HttpsConnector;
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    try_join,
};
use tower::Service;

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

    /// Maximum number of local connections allowed to keep open
    #[arg(long, default_value_t = 4)]
    pool_size: usize,
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

    let client = Client::builder()
        .pool_max_idle_per_host(args.pool_size)
        .build::<_, hyper::Body>(LocalConnector { port: args.port });

    let local_uri = format!("http://localhost:{}", args.port)
        .as_str()
        .parse::<Uri>()
        .unwrap();

    let remote = upgrade::on(res).await?;
    let mut connection = h2::server::handshake(remote).await?;
    while let Some(result) = connection.accept().await {
        let client = client.clone();
        let local_uri = local_uri.clone();
        let (remote_req, mut remote_respond) = result?;

        if remote_req.method() == Method::PATCH {
            println!("Tunneling {remote_req:?}");
            let mut remote_body = remote_req.into_body();
            let local_stream = TcpStream::connect(format!("localhost:{}", args.port)).await?;
            let (mut reader, mut writer) = local_stream.into_split();

            let read = async move {
                println!("Reading from tunnel");
                while let Some(data) = remote_body.data().await {
                    println!("Data from tunnel: {data:?}");
                    let data = data?;
                    remote_body.flow_control().release_capacity(data.len()).ok();
                    writer.write_all(&data).await?;
                }

                println!("Finished reading from tunnel");

                Ok::<_, Box<dyn Error + Send + Sync>>(())
            };

            let res = Response::new(());
            let mut send_stream = remote_respond.send_response(res, false)?;
            println!("Sent response");

            let write = async move {
                println!("Writing to tunnel");
                loop {
                    let mut buf = vec![0; 1024];
                    match reader.read(&mut buf).await? {
                        0 => {
                            println!("Finishing");
                            send_stream.send_data(Bytes::default(), true)?;
                            break;
                        }
                        n => {
                            let buf = BytesMut::from(&buf[0..n]);
                            println!("Sending {buf:?}");
                            send_stream.send_data(buf.into(), false)?;
                        }
                    }
                }
                println!("Closing writing to tunnel");
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            };

            if let Err(err) = try_join!(read, write) {
                println!("Error: {err}");
            }

            return Ok::<_, Box<dyn Error + Send + Sync>>(());
        }

        tokio::spawn(async move {
            let (mut remote_req, mut remote_body) = remote_req.into_parts();
            remove_hop_by_hop_headers(&mut remote_req.headers);
            remote_req.uri = local_uri;
            remote_req.version = Version::HTTP_11;

            let (mut local_body_sender, local_body) = Body::channel();

            let local = async move {
                while let Some(data) = remote_body.data().await {
                    let data = data?;
                    remote_body.flow_control().release_capacity(data.len()).ok();
                    if !data.is_empty() {
                        local_body_sender.send_data(data).await?;
                    }
                }

                Ok::<_, Box<dyn Error + Send + Sync>>(())
            };

            let remote = async move {
                let local_res = client
                    .request(Request::from_parts(remote_req, local_body))
                    .await?;

                let (mut local_res, mut local_res_body) = local_res.into_parts();
                remove_hop_by_hop_headers(&mut local_res.headers);

                let remote_res = hyper::Response::from_parts(local_res, ());
                let mut send = remote_respond.send_response(remote_res, false)?;

                while let Some(data) = local_res_body.data().await {
                    let data = data?;
                    send.send_data(data, false)?;
                }
                send.send_data(Bytes::default(), true)?;
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            };

            if let Err(err) = try_join!(local, remote) {
                println!("Error: {err}");
            }

            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
    }

    Ok(())
}

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    for key in &[
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
        header::ACCEPT_ENCODING,
        header::CONTENT_ENCODING,
    ] {
        headers.remove(key);
    }
}

#[derive(Clone)]
struct LocalConnector {
    port: u16,
}

impl Service<Uri> for LocalConnector {
    type Response = TcpStream;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

    fn poll_ready(&mut self, _: &mut task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: Uri) -> Self::Future {
        Box::pin(TcpStream::connect(format!("localhost:{}", self.port)))
    }
}
