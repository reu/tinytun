use std::{collections::HashMap, error::Error, net::SocketAddr, sync::Arc};

use clap::Parser;
use hyper::{
    client::conn::{self, SendRequest},
    header,
    service::{make_service_fn, service_fn},
    Body, Method, Response, Server, StatusCode,
};
use rand::distributions::{Alphanumeric, DistString};
use tokio::{io, sync::Mutex, try_join};

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

    let conns = Arc::new(Mutex::new(HashMap::<String, SendRequest<_>>::new()));

    let client_server = {
        let conns = conns.clone();
        Server::bind(&SocketAddr::from(([0, 0, 0, 0], args.conn_port))).serve(make_service_fn(
            move |_| {
                let conns = conns.clone();
                async {
                    io::Result::Ok(service_fn(move |mut req| {
                        let conns = conns.clone();
                        async move {
                            if req.method() != Method::CONNECT {
                                return io::Result::Ok(
                                    Response::builder()
                                        .status(StatusCode::METHOD_NOT_ALLOWED)
                                        .header("allow", "connect")
                                        .body(Body::empty())
                                        .unwrap(),
                                );
                            }

                            let conn_id = Alphanumeric
                                .sample_string(&mut rand::thread_rng(), 16)
                                .to_lowercase();

                            {
                                let conn_id = conn_id.clone();
                                tokio::spawn(async move {
                                    if let Ok(conn) = hyper::upgrade::on(&mut req).await {
                                        if let Ok((sender, conn)) = conn::handshake(conn).await {
                                            conns.lock().await.insert(conn_id, sender);
                                            tokio::spawn(async move {
                                                if let Err(err) = conn.await {
                                                    eprintln!("Connection closed {err}");
                                                }
                                            });
                                        }
                                    }
                                });
                            }

                            io::Result::Ok(
                                Response::builder()
                                    .status(StatusCode::SWITCHING_PROTOCOLS)
                                    .header("x-tinytun-connection-id", conn_id)
                                    .body(Body::empty())
                                    .unwrap(),
                            )
                        }
                    }))
                }
            },
        ))
    };

    let proxy = {
        let conns = conns.clone();
        Server::bind(&SocketAddr::from(([0, 0, 0, 0], args.proxy_port))).serve(make_service_fn(
            move |_| {
                let conns = conns.clone();
                async {
                    Ok::<_, Box<dyn Error + Send + Sync>>(service_fn(move |req| {
                        let conns = conns.clone();
                        async move {
                            let host = req
                                .headers()
                                .get(header::HOST)
                                .and_then(|host| host.to_str().ok())
                                .unwrap_or_default();

                            let conn_id = match host.split_once('.').map(|(conn_id, _)| conn_id) {
                                Some(id) => id,
                                None => {
                                    return Ok(Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Body::empty())
                                        .unwrap())
                                }
                            };

                            match conns.lock().await.get_mut(conn_id) {
                                Some(conn) => conn.send_request(req).await,
                                None => Ok(Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Body::empty())
                                    .unwrap()),
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
