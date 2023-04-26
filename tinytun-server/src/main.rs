use std::{
    error::Error,
    io::{BufRead, Cursor},
    net::SocketAddr,
    sync::Arc,
};

use clap::Parser;
use hyper::{
    header,
    service::{make_service_fn, service_fn},
    Body, Method, Response, Server, StatusCode,
};
use tokio::{io::AsyncWriteExt, net::TcpListener, try_join};

use tunnel::Tunnels;

mod tunnel;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Proxy port
    #[arg(short, long, default_value_t = 5555)]
    proxy_port: u16,

    /// Connection management port
    #[arg(short, long, default_value_t = 5554)]
    conn_port: u16,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    let args = Args::parse();

    let tuns = Arc::new(Tunnels::new());

    let client_server = async {
        let tuns = tuns.clone();
        Server::bind(&SocketAddr::from(([0, 0, 0, 0], args.conn_port)))
            .serve(make_service_fn(move |_| {
                let tuns = tuns.clone();
                async {
                    Ok::<_, Box<dyn Error + Send + Sync>>(service_fn(move |mut req| {
                        let tuns = tuns.clone();
                        async move {
                            if req.method() != Method::CONNECT {
                                return Response::builder()
                                    .status(StatusCode::METHOD_NOT_ALLOWED)
                                    .header("allow", "connect")
                                    .body(Body::empty());
                            }

                            let subdomain = req
                                .headers()
                                .get("x-tinytun-subdomain")
                                .and_then(|subdomain| subdomain.to_str().ok())
                                .map(|subdomain| subdomain.to_owned());

                            let mut tun_entry = match tuns.new_tunnel(subdomain.clone()).await {
                                Ok(entry) => entry,
                                Err(_) => {
                                    return Response::builder()
                                        .status(StatusCode::CONFLICT)
                                        .body(Body::from("Subdomain not available"));
                                }
                            };

                            let res = Response::builder()
                                .status(StatusCode::SWITCHING_PROTOCOLS)
                                .header("x-tinytun-connection-id", tun_entry.id().to_string())
                                .header("x-tinytun-subdomain", tun_entry.subdomain())
                                .body(Body::empty())?;

                            tokio::spawn(async move {
                                if let Ok(conn) = hyper::upgrade::on(&mut req).await {
                                    let tun_id = tun_entry.id();

                                    if let Ok((client, h2)) = h2::client::handshake(conn).await {
                                        println!("Conn established");
                                        tun_entry.add_tunnel(client.into()).await;

                                        if let Err(err) = h2.await {
                                            println!("Error: {err}");
                                        }
                                    }

                                    println!("Closing conn");

                                    tuns.remove_tunnel(tun_id).await;
                                }
                            });

                            Ok(res)
                        }
                    }))
                }
            }))
            .await?;

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    let tcp_proxy = async {
        let tuns = tuns.clone();
        let addr = SocketAddr::from(([0, 0, 0, 0], args.proxy_port));
        let listener = TcpListener::bind(&addr).await?;

        while let Ok((mut stream, _addr)) = listener.accept().await {
            let tuns = tuns.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 1024];
                let host = loop {
                    let peeked = stream.peek(&mut buf).await?;
                    if peeked == 0 {
                        return Err("Empty stream".into());
                    }
                    let mut cursor = Cursor::new(&buf);
                    if cursor.read_line(&mut String::with_capacity(peeked / 2))? == 0 {
                        continue;
                    }
                    let position = cursor.position() as usize;
                    drop(cursor);
                    let mut headers = [httparse::EMPTY_HEADER; 10];
                    httparse::parse_headers(&mut buf[position..], &mut headers)?;

                    let host = headers
                        .iter()
                        .find(|header| header.name == header::HOST)
                        .and_then(|header| std::str::from_utf8(header.value).ok());

                    if let Some(host) = host {
                        break host.to_string();
                    }
                };

                let tun_id = match host.split_once('.').map(|(tun_id, _)| tun_id) {
                    Some(id) => id,
                    None => {
                        stream
                            .write_all(b"HTTP/1.1 404\ncontent-length: 0\n\n")
                            .await?;
                        return Ok::<_, Box<dyn Error + Send + Sync>>(());
                    }
                };

                match tuns.tunnel_for_subdomain(tun_id).await {
                    Some(tun) => {
                        println!("Starting tunnel");
                        tun.tunnel(stream).await?;
                        println!("Finishing tunnel");
                    }
                    None => {
                        stream
                            .write_all(b"HTTP/1.1 404\ncontent-length: 0\n\n")
                            .await?;
                    }
                };
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            });
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    let proxy = async {
        let tuns = tuns.clone();
        Server::bind(&SocketAddr::from(([0, 0, 0, 0], args.proxy_port + 1)))
            .serve(make_service_fn(move |_| {
                let tuns = tuns.clone();
                async {
                    Ok::<_, Box<dyn Error + Send + Sync>>(service_fn(move |req| {
                        let tuns = tuns.clone();
                        async move {
                            let host = req
                                .headers()
                                .get(header::HOST)
                                .and_then(|host| host.to_str().ok())
                                .unwrap_or_default();

                            let tun_id = match host.split_once('.').map(|(tun_id, _)| tun_id) {
                                Some(id) => id,
                                None => {
                                    return Ok(Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Body::empty())?)
                                }
                            };

                            match tuns.tunnel_for_subdomain(tun_id).await {
                                Some(tun) => tun.request(req).await,
                                None => Ok(Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Body::empty())?),
                            }
                        }
                    }))
                }
            }))
            .await?;

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    try_join!(client_server, proxy, tcp_proxy)?;

    Ok(())
}
