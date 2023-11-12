use std::{
    env,
    error::Error,
    io::{BufRead, Cursor},
    net::SocketAddr,
    sync::Arc,
    time::Duration,
};

use hyper::{
    header,
    server::conn::{AddrIncoming, Http as HttpServer},
    service::{make_service_fn, service_fn},
    Body, Method, Request, Response, Server, StatusCode,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    time::sleep,
    try_join,
};

use tracing::{debug, info, metadata::LevelFilter, trace, Instrument};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use tunnel::{Tunnel, TunnelName, Tunnels};

mod tunnel;

async fn http_tunnel(
    base_domain: &str,
    tuns: Arc<Tunnels>,
    mut req: Request<Body>,
) -> Result<Response<Body>, Box<dyn Error + Send + Sync>> {
    let subdomain = req
        .headers()
        .get("x-tinytun-subdomain")
        .and_then(|subdomain| subdomain.to_str().ok())
        .map(|subdomain| subdomain.to_owned());

    let mut tun_entry = match tuns.new_http_tunnel(subdomain.clone()).await {
        Ok(entry) => entry,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from("Subdomain not available"))?);
        }
    };

    let subdomain = tun_entry.name().to_string();
    let tun_id = tun_entry.id();

    let res = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("x-tinytun-connection-id", tun_id.to_string())
        .header("x-tinytun-domain", format!("{subdomain}.{base_domain}"))
        .header("x-tinytun-subdomain", &subdomain)
        .body(Body::empty())?;

    tokio::spawn(
        async move {
            if let Ok(conn) = hyper::upgrade::on(&mut req).await {
                trace!("Tunnel opened");

                if let Ok((client, mut h2)) = h2::client::handshake(conn).await {
                    tun_entry.add_tunnel(client.into()).await;

                    let mut ping_pong = h2.ping_pong().unwrap();
                    #[allow(unreachable_code)]
                    let ping = async move {
                        loop {
                            sleep(Duration::from_secs(10)).await;
                            trace!("Sending ping");
                            ping_pong.ping(h2::Ping::opaque()).await?;
                            trace!("Received pong");
                        }
                        Ok::<_, h2::Error>(())
                    }
                    .in_current_span();

                    if let Err(err) = tokio::select! {
                        result = h2 => result,
                        result = ping => result,
                    } {
                        debug!(error = %err, "Error");
                    }
                }

                tuns.remove_tunnel(tun_id).await;
                trace!("Tunnel closed");
            }
        }
        .instrument(tracing::error_span!("Tunnel", kind = "http", %subdomain)),
    );

    Ok(res)
}

async fn tcp_tunnel(
    tuns: Arc<Tunnels>,
    mut req: Request<Body>,
) -> Result<Response<Body>, Box<dyn Error + Send + Sync>> {
    let port = req
        .headers()
        .get("x-tinytun-port")
        .and_then(|port| port.to_str().ok())
        .and_then(|port| port.parse::<u16>().ok());

    let (tun_id, port) = match tuns.new_tcp_tunnel(port).await {
        Ok(entry) => entry,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from("Port not available"))?);
        }
    };

    let res = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("x-tinytun-connection-id", tun_id.to_string())
        .header("x-tinytun-port", port)
        .body(Body::empty())?;

    tokio::spawn(
        async move {
            if let Ok(conn) = hyper::upgrade::on(&mut req).await {
                trace!("Tunnel opened");

                if let Ok((client, mut h2)) = h2::client::handshake(conn).await {
                    let tun: Tunnel = client.into();
                    let listener = async move {
                        let addr = SocketAddr::from(([0, 0, 0, 0], port));
                        let listener = TcpListener::bind(&addr)
                            .await
                            .map_err(|_err| h2::Error::from(h2::Reason::INTERNAL_ERROR))?;
                        while let Ok((stream, _addr)) = listener.accept().await {
                            let tun = tun.clone();
                            tokio::spawn(async move {
                                tun.tunnel(stream).await?;
                                Ok::<_, Box<dyn Error + Send + Sync>>(())
                            });
                        }
                        Ok::<_, h2::Error>(())
                    };

                    let mut ping_pong = h2.ping_pong().unwrap();
                    #[allow(unreachable_code)]
                    let ping = async move {
                        loop {
                            sleep(Duration::from_secs(10)).await;
                            trace!("Sending ping");
                            ping_pong.ping(h2::Ping::opaque()).await?;
                            trace!("Received pong");
                        }
                        Ok::<_, h2::Error>(())
                    }
                    .in_current_span();

                    if let Err(err) = tokio::select! {
                        result = h2 => result,
                        result = ping => result,
                        result = listener => result,
                    } {
                        debug!(error = %err, "Error");
                    }
                }

                tuns.remove_tunnel(tun_id).await;
                trace!("Tunnel closed");
            }
        }
        .instrument(tracing::error_span!("Tunnel", kind = "tcp", %port)),
    );

    Ok(res)
}

async fn tcp_proxy_tunnel(
    tuns: Arc<Tunnels>,
    mut req: Request<Body>,
) -> Result<Response<Body>, Box<dyn Error + Send + Sync>> {
    let port = req
        .headers()
        .get("x-tinytun-port")
        .and_then(|port| port.to_str().ok())
        .and_then(|port| port.parse::<u16>().ok());

    let (mut tun_entry, port) = match tuns.new_tcp_proxy_tunnel(port).await {
        Ok(entry) => entry,
        Err(_) => {
            return Ok(Response::builder()
                .status(StatusCode::CONFLICT)
                .body(Body::from("Port not available"))?);
        }
    };

    let tun_id = tun_entry.id();

    let res = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header("x-tinytun-connection-id", tun_id.to_string())
        .header("x-tinytun-port", port)
        .body(Body::empty())?;

    tokio::spawn(
        async move {
            if let Ok(conn) = hyper::upgrade::on(&mut req).await {
                trace!("Tunnel opened");

                if let Ok((client, mut h2)) = h2::client::handshake(conn).await {
                    tun_entry.add_tunnel(client.into()).await;

                    let mut ping_pong = h2.ping_pong().unwrap();
                    #[allow(unreachable_code)]
                    let ping = async move {
                        loop {
                            sleep(Duration::from_secs(10)).await;
                            trace!("Sending ping");
                            ping_pong.ping(h2::Ping::opaque()).await?;
                            trace!("Received pong");
                        }
                        Ok::<_, h2::Error>(())
                    }
                    .in_current_span();

                    if let Err(err) = tokio::select! {
                        result = h2 => result,
                        result = ping => result,
                    } {
                        debug!(error = %err, "Error");
                    }
                }

                tuns.remove_tunnel(tun_id).await;
                trace!("Tunnel closed");
            }
        }
        .instrument(tracing::error_span!("Tunnel", kind = "tcp", %port)),
    );

    Ok(res)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::ERROR.into())
                .from_env_lossy(),
        )
        .init();

    let metadata_port = env::var("METADATA_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5553);

    let conn_port = env::var("CONNECTION_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5554);

    let proxy_port = env::var("PROXY_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5555);

    let tcp_proxy_port = env::var("TCP_PROXY_PORT")
        .ok()
        .and_then(|port| port.parse::<u16>().ok())
        .unwrap_or(5556);

    let base_domain = env::var("BASE_DOMAIN").ok().unwrap_or_else(|| {
        option_env!("DEFAULT_BASE_DOMAIN")
            .unwrap_or("local.tinytun.com")
            .to_string()
    });
    let base_domain = Arc::new(base_domain);

    let tuns = Arc::new(Tunnels::new());

    let api = async {
        let tuns = tuns.clone();

        let addr = SocketAddr::from(([0, 0, 0, 0], conn_port));
        let listener = TcpListener::bind(&addr).await?;
        info!("API running on port {}", conn_port);

        Server::builder(AddrIncoming::from_listener(listener)?)
            .serve(make_service_fn(move |_| {
                let base_domain = base_domain.clone();
                let tuns = tuns.clone();
                async {
                    Ok::<_, Box<dyn Error + Send + Sync>>(service_fn(move |req| {
                        debug!("New tunnel request");
                        let tuns = tuns.clone();
                        let base_domain = base_domain.clone();

                        async move {
                            if req.method() != Method::CONNECT {
                                return Ok(Response::builder()
                                    .status(StatusCode::OK)
                                    .body(Body::empty())?);
                            }

                            let tunnel_type = req
                                .headers()
                                .get("x-tinytun-type")
                                .and_then(|header| header.to_str().ok());

                            debug!(tunnel_type, "Tunnel type creation requested");

                            match tunnel_type {
                                Some("tcp") => Ok::<_, Box<dyn Error + Send + Sync>>(
                                    tcp_proxy_tunnel(tuns, req).await?,
                                ),
                                Some("tcp_port") => Ok::<_, Box<dyn Error + Send + Sync>>(
                                    tcp_tunnel(tuns, req).await?,
                                ),
                                _ => Ok::<_, Box<dyn Error + Send + Sync>>(
                                    http_tunnel(&base_domain, tuns, req).await?,
                                ),
                            }
                        }
                    }))
                }
            }))
            .await?;

        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    let metadata_api = async {
        let tuns = tuns.clone();
        let addr = SocketAddr::from(([0, 0, 0, 0], metadata_port));
        let listener = TcpListener::bind(&addr).await?;
        info!("Metadata api running on port {}", metadata_port);

        Server::builder(AddrIncoming::from_listener(listener)?)
            .serve(make_service_fn(move |_| {
                let tuns = tuns.clone();

                async move {
                    Ok::<_, Box<dyn Error + Send + Sync>>(service_fn(move |req| {
                        let tuns = tuns.clone();
                        async move {
                            match req.uri().path().split('/').collect::<Vec<_>>().as_slice() {
                                ["", "tunnels"] => {
                                    let tunnels =
                                        tuns.list_tunnels_metadata().await.collect::<Vec<_>>();

                                    let tunnels = serde_json::to_vec(&tunnels).unwrap();

                                    Response::builder()
                                        .status(StatusCode::OK)
                                        .body(Body::from(tunnels))
                                }

                                _ => Response::builder()
                                    .status(StatusCode::NOT_FOUND)
                                    .body(Body::empty()),
                            }
                        }
                    }))
                }
            }))
            .await?;
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    let http_proxy = async {
        let tuns = tuns.clone();
        let addr = SocketAddr::from(([0, 0, 0, 0], proxy_port));
        let listener = TcpListener::bind(&addr).await?;
        info!("Proxy running on port {}", proxy_port);

        while let Ok((stream, _addr)) = listener.accept().await {
            let tuns = tuns.clone();
            tokio::spawn(async move {
                let host = peek_host(&stream).await?;

                let subdomain = match host.split_once('.').map(|(tun_id, _)| tun_id) {
                    Some(id) => TunnelName::Subdomain(id.to_string()),
                    None => {
                        HttpServer::new()
                            .serve_connection(
                                stream,
                                service_fn(|_| async {
                                    Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Body::from("Tunnel not informed"))
                                }),
                            )
                            .await?;
                        return Ok::<_, Box<dyn Error + Send + Sync>>(());
                    }
                };

                match tuns.tunnel_for_name(&subdomain).await {
                    Some(tun) => {
                        tun.tunnel(stream)
                            .instrument(tracing::error_span!(
                                "Tunneling",
                                subdomain = subdomain.to_string()
                            ))
                            .await?;
                    }
                    None => {
                        HttpServer::new()
                            .serve_connection(
                                stream,
                                service_fn(|_| async {
                                    Response::builder()
                                        .status(StatusCode::NOT_FOUND)
                                        .body(Body::from("Tunnel not found"))
                                }),
                            )
                            .await?;
                    }
                };
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            });
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    let tcp_proxy = async {
        let tuns = tuns.clone();
        let addr = SocketAddr::from(([0, 0, 0, 0], tcp_proxy_port));
        let listener = TcpListener::bind(&addr).await?;
        info!("TCP Proxy running on port {}", tcp_proxy_port);

        while let Ok((mut stream, _addr)) = listener.accept().await {
            debug!("TCP stream accepted");
            let tuns = tuns.clone();
            tokio::spawn(async move {
                let port = match parse_proxy_protocol(&mut stream).await {
                    Ok(port) => TunnelName::PortNumber(port),
                    Err(err) => {
                        stream.shutdown().await?;
                        return Err(err);
                    }
                };
                debug!(port = port.to_string(), "TCP port received");

                match tuns.tunnel_for_name(&port).await {
                    Some(tun) => {
                        debug!(port = port.to_string(), "TCP tunnel found");
                        tun.tunnel(stream)
                            .instrument(tracing::error_span!(
                                "TCP Tunneling",
                                port = port.to_string()
                            ))
                            .await?;
                    }
                    None => {
                        debug!(port = port.to_string(), "TCP stream not found");
                        stream.shutdown().await?;
                    }
                };
                Ok::<_, Box<dyn Error + Send + Sync>>(())
            });
        }
        Ok::<_, Box<dyn Error + Send + Sync>>(())
    };

    try_join!(api, http_proxy, tcp_proxy, metadata_api)?;

    Ok(())
}

async fn peek_host(stream: &TcpStream) -> Result<String, Box<dyn Error + Send + Sync>> {
    let mut buf = vec![0; 1024];
    loop {
        let peeked = stream.peek(&mut buf).await?;
        if peeked == 0 {
            return Err("Empty stream".into());
        }
        let mut cursor = Cursor::new(&buf);
        if cursor.read_line(&mut String::with_capacity(peeked / 2))? == 0 {
            continue;
        }
        let position = cursor.position().try_into()?;
        let mut headers = [httparse::EMPTY_HEADER; 16];
        httparse::parse_headers(&buf[position..], &mut headers)?;

        let host = headers
            .iter()
            .find(|header| header.name == header::HOST)
            .and_then(|header| std::str::from_utf8(header.value).ok());

        if let Some(host) = host {
            break Ok(host.to_string());
        }
    }
}

// See: https://www.haproxy.org/download/2.4/doc/proxy-protocol.txt
async fn parse_proxy_protocol(stream: &mut TcpStream) -> Result<u16, Box<dyn Error + Send + Sync>> {
    let mut header = [0_u8; 16];
    stream.read_exact(&mut header).await?;

    if &header[0..12] != b"\x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A" {
        return Err("invalid proxy protocol v2 signature".into());
    }

    let len = u16::from_be_bytes([header[header.len() - 2], header[header.len() - 1]]);
    debug!(len, "proxy protocol lenght");

    let mut buf = vec![0; len.into()];
    stream.read_exact(&mut buf).await?;

    // We are just interested in the destination port
    let port = u16::from_be_bytes([buf[10], buf[11]]);
    debug!(port, "proxy protocol parsed");

    Ok(port)
}
