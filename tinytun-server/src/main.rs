use std::{error::Error, net::SocketAddr, sync::Arc};

use clap::Parser;
use hyper::{
    client::conn,
    header,
    service::{make_service_fn, service_fn},
    Body, Method, Response, Server, StatusCode,
};
use tokio::{io::AsyncReadExt, try_join};

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

    let client_server = {
        let tuns = tuns.clone();
        Server::bind(&SocketAddr::from(([0, 0, 0, 0], args.conn_port))).serve(make_service_fn(
            move |_| {
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
                                if let Ok(mut conn) = hyper::upgrade::on(&mut req).await {
                                    let tun_id = tun_entry.id();

                                    'outer: loop {
                                        if let Ok((sender, tun)) = conn::handshake(conn).await {
                                            tun_entry.add_tunnel(sender.into()).await;
                                            match tun.without_shutdown().await {
                                                Ok(parts) => {
                                                    let mut stream = parts.io;
                                                    loop {
                                                        match stream.read(&mut [0; 1024]).await {
                                                            Ok(bytes) if bytes < 1024 => break,
                                                            Err(err) => {
                                                                eprintln!("closed {err}");
                                                                break 'outer;
                                                            }
                                                            _ => continue,
                                                        }
                                                    }
                                                    conn = stream;
                                                    continue;
                                                }
                                                Err(err) => {
                                                    eprintln!("closed {err}");
                                                    break;
                                                }
                                            }
                                        }
                                        break;
                                    }
                                    tuns.remove_tunnel(tun_id).await;
                                }
                            });

                            Ok(res)
                        }
                    }))
                }
            },
        ))
    };

    let proxy = {
        let tuns = tuns.clone();
        Server::bind(&SocketAddr::from(([0, 0, 0, 0], args.proxy_port))).serve(make_service_fn(
            move |_| {
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
            },
        ))
    };

    try_join!(client_server, proxy)?;

    Ok(())
}
