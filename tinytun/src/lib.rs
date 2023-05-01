use std::{
    cmp::min,
    error::Error,
    pin::Pin,
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
    proxy_url: Uri,
    connection: Connection<Upgraded, Bytes>,
}

impl Tunnel {
    pub fn builder() -> TunnelBuilder {
        TunnelBuilder::default()
    }

    pub fn proxy_url(&self) -> &Uri {
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

pub struct TunnelBuilder {
    server_url: Result<Uri, Box<dyn Error + Send + Sync>>,
    base_url: Result<Uri, Box<dyn Error + Send + Sync>>,
    subdomain: Option<String>,
    max_concurrent_streams: u32,
}

impl Default for TunnelBuilder {
    fn default() -> Self {
        Self {
            server_url: Ok(Uri::from_static("https://control.tinytun.com:5555")),
            base_url: Ok(Uri::from_static("https://tinytun.com")),
            max_concurrent_streams: 100,
            subdomain: Default::default(),
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

    pub fn base_url<T>(self, base_url: T) -> Self
    where
        Uri: TryFrom<T>,
        <Uri as TryFrom<T>>::Error: Into<Box<dyn Error + Send + Sync>>,
    {
        Self {
            base_url: base_url.try_into().map_err(Into::into),
            ..self
        }
    }

    pub fn subdomain(self, subdomain: impl Into<Option<String>>) -> Self {
        Self {
            subdomain: subdomain.into(),
            ..self
        }
    }

    pub fn max_concurrent_streams(self, streams: u32) -> Self {
        Self {
            max_concurrent_streams: streams,
            ..self
        }
    }

    pub async fn listen(self) -> Result<Tunnel, Box<dyn Error + Send + Sync>> {
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
                    .uri(self.server_url?)
                    .method(Method::CONNECT);

                match self.subdomain {
                    Some(subdomain) if !subdomain.trim().is_empty() => req
                        .header("x-tinytun-subdomain", subdomain)
                        .body(Body::empty())?,
                    _ => req.body(Body::empty())?,
                }
            })
            .await?;

        let conn_id = res
            .headers()
            .get("x-tinytun-subdomain")
            .ok_or("Server didn't provide a connection id")?
            .to_str()?;

        let proxy_url = Uri::from_parts({
            let mut proxy_url = self.base_url?.into_parts();
            proxy_url.authority = {
                let authority = format!("{conn_id}.{}", proxy_url.authority.unwrap());
                Some(authority.parse()?)
            };
            proxy_url
        })?;

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
