use std::{
    cmp::min,
    error::Error,
    fmt::Display,
    pin::Pin,
    str::FromStr,
    task::{self, Poll},
};

use bytes::{Buf, Bytes};
use h2::{server::Connection, Reason, RecvStream, SendStream};
use hyper::{
    upgrade::{self, Upgraded},
    Body, Client, Method, Request, Response, Uri,
};
use hyper_rustls::HttpsConnectorBuilder;
use tokio::io::{self, AsyncRead, AsyncWrite};

pub struct Tunnel {
    proxy_url: String,
    connection: Connection<Upgraded, Bytes>,
}

impl Tunnel {
    pub fn builder() -> TunnelBuilder {
        TunnelBuilder::default()
    }

    pub fn proxy_url(&self) -> &str {
        &self.proxy_url
    }

    pub async fn accept(&mut self) -> Option<TunnelStream> {
        match self.connection.accept().await {
            Some(Ok((req, mut respond))) => {
                let sender = respond.send_response(Response::new(()), false).ok()?;
                Some(TunnelStream::new(req.into_body(), sender))
            }
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TunnelType {
    Tcp,
    Http,
}

impl Display for TunnelType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelType::Tcp => write!(f, "tcp"),
            TunnelType::Http => write!(f, "http"),
        }
    }
}

impl FromStr for TunnelType {
    type Err = &'static str;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "tcp" => Ok(TunnelType::Tcp),
            "http" => Ok(TunnelType::Http),
            _ => Err("invalid tunnel type"),
        }
    }
}

pub struct TunnelBuilder {
    server_url: Result<Uri, Box<dyn Error + Send + Sync>>,
    subdomain: Option<String>,
    port: Option<u16>,
    max_concurrent_streams: u32,
    tunnel_type: TunnelType,
}

impl Default for TunnelBuilder {
    fn default() -> Self {
        Self {
            server_url: Ok(Uri::from_static("https://tinytun.com:5555")),
            max_concurrent_streams: 100,
            subdomain: Default::default(),
            port: Default::default(),
            tunnel_type: TunnelType::Http,
        }
    }
}

impl TunnelBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn server_url<T>(self, server_url: T) -> Self
    where
        Uri: TryFrom<T>,
        <Uri as TryFrom<T>>::Error: Into<Box<dyn Error + Send + Sync>>,
    {
        Self {
            server_url: server_url.try_into().map_err(Into::into),
            ..self
        }
    }

    pub fn subdomain(self, subdomain: impl Into<Option<String>>) -> Self {
        Self {
            subdomain: subdomain.into(),
            ..self
        }
    }

    pub fn port(self, port: impl Into<Option<u16>>) -> Self {
        Self {
            port: port.into(),
            ..self
        }
    }

    pub fn max_concurrent_streams(self, streams: u32) -> Self {
        Self {
            max_concurrent_streams: streams,
            ..self
        }
    }

    pub fn tunnel_type(self, tunnel: impl Into<TunnelType>) -> Self {
        Self {
            tunnel_type: tunnel.into(),
            ..self
        }
    }

    pub async fn listen(self) -> Result<Tunnel, Box<dyn Error + Send + Sync>> {
        let server_url = self.server_url?;
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
                    .uri(&server_url)
                    .method(Method::CONNECT)
                    .header("x-tinytun-type", self.tunnel_type.to_string());

                match self.tunnel_type {
                    TunnelType::Tcp => match self.port {
                        Some(port) => req.header("x-tinytun-port", port).body(Body::empty())?,
                        _ => req.body(Body::empty())?,
                    },
                    TunnelType::Http => match self.subdomain {
                        Some(subdomain) if !subdomain.trim().is_empty() => req
                            .header("x-tinytun-subdomain", subdomain)
                            .body(Body::empty())?,
                        _ => req.body(Body::empty())?,
                    },
                }
            })
            .await?;

        let proxy_url = match self.tunnel_type {
            TunnelType::Tcp => {
                let port = res
                    .headers()
                    .get("x-tinytun-port")
                    .ok_or("Server didn't provide a connection port")?
                    .to_str()?;

                format!("{}:{port}", server_url.authority().unwrap().host())
            }
            TunnelType::Http => {
                let domain = res
                    .headers()
                    .get("x-tinytun-domain")
                    .ok_or("Server didn't provide a connection id")?
                    .to_str()?;

                Uri::builder()
                    .scheme(
                        server_url
                            .scheme()
                            .map(|scheme| scheme.to_string())
                            .unwrap_or("http".to_string())
                            .as_str(),
                    )
                    .authority(domain)
                    .path_and_query("")
                    .build()?
                    .to_string()
            }
        };

        let remote = upgrade::on(res).await?;
        let connection = h2::server::Builder::new()
            .max_concurrent_streams(self.max_concurrent_streams)
            .handshake(remote)
            .await?;

        Ok(Tunnel {
            proxy_url,
            connection,
        })
    }
}

pub struct TunnelStream {
    receiver: RecvStream,
    sender: SendStream<Bytes>,
    buf: Bytes,
}

impl TunnelStream {
    pub fn new(receiver: RecvStream, sender: SendStream<Bytes>) -> Self {
        Self {
            sender,
            receiver,
            buf: Bytes::new(),
        }
    }
}

impl AsyncRead for TunnelStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.buf.is_empty() {
            self.buf = loop {
                match task::ready!(self.receiver.poll_data(cx)) {
                    Some(Ok(buf)) if buf.is_empty() && !self.receiver.is_end_stream() => continue,
                    Some(Ok(buf)) => break buf,
                    Some(Err(err)) => {
                        return Poll::Ready(match err.reason() {
                            Some(Reason::NO_ERROR) | Some(Reason::CANCEL) => Ok(()),
                            Some(Reason::STREAM_CLOSED) => {
                                Err(io::Error::new(io::ErrorKind::BrokenPipe, err))
                            }
                            _ => Err(h2_error_to_io_error(err)),
                        })
                    }
                    None => return Poll::Ready(Ok(())),
                }
            };
        }

        let len = min(self.buf.len(), buf.remaining());
        buf.put_slice(&self.buf[..len]);
        self.buf.advance(len);
        self.receiver.flow_control().release_capacity(len).ok();

        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for TunnelStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        self.sender.reserve_capacity(buf.len());

        let written = match task::ready!(self.sender.poll_capacity(cx)) {
            Some(Ok(capacity)) => self
                .sender
                // TODO: try to figure out a way to avoid this copy
                .send_data(Bytes::copy_from_slice(&buf[..capacity]), false)
                .ok()
                .map(|_| capacity),
            Some(Err(_)) => None,
            None => Some(0),
        };

        if let Some(len) = written {
            return Poll::Ready(Ok(len));
        }

        match task::ready!(self.sender.poll_reset(cx)) {
            Ok(Reason::NO_ERROR) | Ok(Reason::CANCEL) | Ok(Reason::STREAM_CLOSED) => {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            }
            Ok(reason) => Poll::Ready(Err(h2_error_to_io_error(reason.into()))),
            Err(err) => Poll::Ready(Err(h2_error_to_io_error(err))),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<io::Result<()>> {
        if self.sender.send_data(Bytes::new(), true).is_ok() {
            return Poll::Ready(Ok(()));
        }

        match task::ready!(self.sender.poll_reset(cx)) {
            Ok(Reason::NO_ERROR) => Poll::Ready(Ok(())),
            Ok(Reason::CANCEL) | Ok(Reason::STREAM_CLOSED) => {
                Poll::Ready(Err(io::ErrorKind::BrokenPipe.into()))
            }
            Ok(reason) => Poll::Ready(Err(h2_error_to_io_error(reason.into()))),
            Err(err) => Poll::Ready(Err(h2_error_to_io_error(err))),
        }
    }
}

fn h2_error_to_io_error(err: h2::Error) -> io::Error {
    if err.is_io() {
        err.into_io().unwrap()
    } else {
        io::Error::new(io::ErrorKind::Other, err)
    }
}
