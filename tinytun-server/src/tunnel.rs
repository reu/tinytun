use std::{
    error::Error,
    fmt::Display,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    task::{self, ready},
};

use bytes::Bytes;
use dashmap::DashMap;
use hyper::Request;
use pin_project_lite::pin_project;
use rand::Rng;
use serde::{Serialize, Serializer};
use tinytun::TunnelStream;
use tokio::{
    io::{self, AsyncRead, AsyncWrite},
    net::TcpStream,
    time::{timeout, Duration},
};
use tracing::instrument;
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct Tunnel {
    client: h2::client::SendRequest<Bytes>,
    read_bytes: Arc<AtomicUsize>,
    written_bytes: Arc<AtomicUsize>,
}

impl Tunnel {
    #[instrument(skip(self, stream))]
    pub async fn tunnel(&self, mut stream: TcpStream) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut client = timeout(Duration::from_secs(5), self.client.clone().ready())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "H2 stream not ready"))??;
        let (res, send_stream) = client.send_request(Request::new(()), false)?;
        let res = timeout(Duration::from_secs(10), res)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "H2 send_request timed out"))??;
        let tunnel_stream = TunnelStream::new(res.into_body(), send_stream);
        let mut tunnel_stream = StreamMonitor {
            inner: tunnel_stream,
            read_bytes: self.read_bytes.clone(),
            written_bytes: self.written_bytes.clone(),
        };
        io::copy_bidirectional(&mut stream, &mut tunnel_stream).await?;
        Ok(())
    }
}

impl From<h2::client::SendRequest<Bytes>> for Tunnel {
    fn from(send_request: h2::client::SendRequest<Bytes>) -> Self {
        Tunnel {
            client: send_request,
            read_bytes: Default::default(),
            written_bytes: Default::default(),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct TunnelId(Uuid);

impl TunnelId {
    pub fn new() -> Self {
        TunnelId(Uuid::new_v4())
    }
}

impl Display for TunnelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.as_simple())
    }
}

impl Serialize for TunnelId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Serialize)]
pub enum TunnelName {
    Subdomain(String),
    PortNumber(u16),
}

impl Display for TunnelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelName::Subdomain(name) => write!(f, "{}", name),
            TunnelName::PortNumber(num) => write!(f, ":{}", num),
        }
    }
}

pub struct Tunnels {
    tunnels: DashMap<TunnelId, (Tunnel, TunnelName)>,
    names: DashMap<TunnelName, TunnelId>,
}

impl Tunnels {
    pub fn new() -> Self {
        Self {
            tunnels: DashMap::new(),
            names: DashMap::new(),
        }
    }

    pub async fn new_http_tunnel(
        self: &Arc<Self>,
        name: Option<String>,
    ) -> Result<TunnelEntry, Box<dyn Error + Send + Sync>> {
        let tun_id = TunnelId::new();
        let name = TunnelName::Subdomain(name.unwrap_or_else(|| tun_id.to_string()));

        if self.names.contains_key(&name) {
            return Err("Name already in use".into());
        }

        self.names.insert(name.clone(), tun_id);

        Ok(TunnelEntry {
            tun_id,
            name,
            registry: self.clone(),
            persisted: false,
        })
    }

    fn resolve_port(&self, port: Option<u16>) -> Result<(TunnelName, u16), Box<dyn Error + Send + Sync>> {
        match port {
            Some(port) => {
                let name = TunnelName::PortNumber(port);
                if self.names.contains_key(&name) {
                    return Err("Port already in use".into());
                }
                Ok((name, port))
            }
            None => {
                for _ in 0..100 {
                    let port = rand::thread_rng().gen_range(20_000..60_000);
                    let name = TunnelName::PortNumber(port);
                    if !self.names.contains_key(&name) {
                        return Ok((name, port));
                    }
                }
                Err("No available port found after 100 attempts".into())
            }
        }
    }

    pub async fn new_tcp_proxy_tunnel(
        self: &Arc<Self>,
        port: Option<u16>,
    ) -> Result<(TunnelEntry, u16), Box<dyn Error + Send + Sync>> {
        let tun_id = TunnelId::new();
        let (name, port) = self.resolve_port(port)?;

        self.names.insert(name.clone(), tun_id);

        Ok((
            TunnelEntry {
                tun_id,
                name,
                registry: self.clone(),
                persisted: false,
            },
            port,
        ))
    }

    pub async fn new_tcp_tunnel(
        self: &Arc<Self>,
        port: Option<u16>,
    ) -> Result<(TunnelId, u16), Box<dyn Error + Send + Sync>> {
        let tun_id = TunnelId::new();
        let (name, port) = self.resolve_port(port)?;

        self.names.insert(name.clone(), tun_id);

        Ok((tun_id, port))
    }

    pub async fn tunnel_for_name(&self, name: &TunnelName) -> Option<Tunnel> {
        let tun_id = self.names.get(name)?;
        self.tunnels.get(&tun_id).map(|tun| tun.value().0.clone())
    }

    pub async fn remove_tunnel(&self, tun_id: TunnelId) {
        if let Some((_, (_, name))) = self.tunnels.remove(&tun_id) {
            self.names.remove(&name);
        }
    }

    pub async fn list_tunnels_metadata(&self) -> impl Iterator<Item = TunnelMetadata> + '_ {
        self.tunnels.iter().map(|item| {
            let (tunnel, name) = item.value();
            TunnelMetadata {
                id: *item.key(),
                name: name.clone(),
                read_bytes: tunnel.read_bytes.load(Ordering::Relaxed),
                written_bytes: tunnel.written_bytes.load(Ordering::Relaxed),
            }
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelMetadata {
    pub id: TunnelId,
    pub name: TunnelName,
    pub read_bytes: usize,
    pub written_bytes: usize,
}

pub struct TunnelEntry {
    tun_id: TunnelId,
    name: TunnelName,
    registry: Arc<Tunnels>,
    persisted: bool,
}

impl TunnelEntry {
    pub fn id(&self) -> TunnelId {
        self.tun_id
    }

    pub fn name(&self) -> &TunnelName {
        &self.name
    }

    pub async fn add_tunnel(&mut self, tunnel: Tunnel) {
        self.registry
            .tunnels
            .insert(self.tun_id, (tunnel.clone(), self.name.clone()));
        self.persisted = true;
    }
}

impl Drop for TunnelEntry {
    fn drop(&mut self) {
        if !self.persisted {
            let subdomain = self.name.clone();
            let tunnels = self.registry.clone();
            tunnels.names.remove(&subdomain);
        }
    }
}

pin_project! {
    struct StreamMonitor<T> {
        #[pin]
        inner: T,
        read_bytes: Arc<AtomicUsize>,
        written_bytes: Arc<AtomicUsize>,
    }
}

impl<T: AsyncRead> AsyncRead for StreamMonitor<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut io::ReadBuf<'_>,
    ) -> task::Poll<io::Result<()>> {
        let this = self.project();
        let remaining = buf.remaining();
        let result = this.inner.poll_read(cx, buf);
        let read = remaining - buf.remaining();
        this.read_bytes.fetch_add(read, Ordering::Relaxed);
        result
    }
}

impl<T: AsyncWrite> AsyncWrite for StreamMonitor<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &[u8],
    ) -> task::Poll<io::Result<usize>> {
        let this = self.project();
        let written = ready!(this.inner.poll_write(cx, buf))?;
        this.written_bytes.fetch_add(written, Ordering::Relaxed);
        task::Poll::Ready(Ok(written))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> task::Poll<io::Result<()>> {
        self.project().inner.poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
    ) -> task::Poll<io::Result<()>> {
        self.project().inner.poll_shutdown(cx)
    }
}
