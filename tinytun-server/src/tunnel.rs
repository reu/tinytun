use std::{
    collections::HashMap,
    fmt::Display,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, RwLock,
    },
    task::{self, ready},
    time::Instant,
};

use bytes::Bytes;
use hyper::{Request, Uri};
use pin_project_lite::pin_project;
use rand::Rng;
use serde::{Serialize, Serializer};
use thiserror::Error;
use tinytun::TunnelStream;
use tokio::{
    io::{self, AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{self, TcpStream},
    time::{timeout, Duration},
};
use tracing::instrument;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RegistryError {
    #[error("subdomain already in use")]
    SubdomainConflict,
    #[error("port already in use: {0}")]
    PortConflict(u16),
    #[error("no available port found after 100 attempts")]
    PortExhausted,
}

#[derive(Debug, Error)]
pub enum TunnelError {
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    H2(#[from] h2::Error),
}

const CLIENT_READY_TIMEOUT: Duration = Duration::from_secs(5);
const SEND_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PORT_RANGE: std::ops::Range<u16> = 20_000..60_000;
const MAX_PORT_RETRIES: usize = 100;
const PEER_QUERY_TIMEOUT: Duration = Duration::from_secs(2);
const PEER_CACHE_TTL: Duration = Duration::from_secs(10);
const PEER_CACHE_NEGATIVE_TTL: Duration = Duration::from_secs(2);
const PEER_CACHE_MAX_ENTRIES: usize = 4096;

pub trait PeerResolver: Send + Sync {
    fn resolve(&self) -> impl std::future::Future<Output = Vec<SocketAddr>> + Send;
}

pub struct DnsPeerResolver {
    hostname: String,
    port: u16,
}

impl DnsPeerResolver {
    pub fn new(hostname: impl Into<String>, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            port,
        }
    }
}

impl PeerResolver for DnsPeerResolver {
    async fn resolve(&self) -> Vec<SocketAddr> {
        net::lookup_host((&*self.hostname, self.port))
            .await
            .map(|addrs| addrs.collect())
            .unwrap_or_default()
    }
}

impl PeerResolver for Vec<SocketAddr> {
    async fn resolve(&self) -> Vec<SocketAddr> {
        self.clone()
    }
}

#[derive(Debug, Clone)]
enum TunnelConnection {
    Local(h2::client::SendRequest<Bytes>),
    Remote {
        addr: SocketAddr,
        /// When set, a PROXY protocol v2 header advertising this destination
        /// port is written before piping bytes, so the peer's TCP proxy can
        /// route the connection. The original header was already consumed
        /// while parsing it on this server, so it must be reconstructed.
        proxy_protocol_port: Option<u16>,
    },
}

#[derive(Debug, Clone)]
pub struct Tunnel {
    connection: TunnelConnection,
    read_bytes: Arc<AtomicUsize>,
    written_bytes: Arc<AtomicUsize>,
}

impl Tunnel {
    #[instrument(skip(self, stream))]
    pub async fn tunnel(&self, mut stream: TcpStream) -> Result<(), TunnelError> {
        match self.connection {
            TunnelConnection::Local(ref client) => {
                let mut client = timeout(CLIENT_READY_TIMEOUT, client.clone().ready())
                    .await
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::TimedOut, "H2 stream not ready")
                    })??;

                let (res, send_stream) = client.send_request(Request::new(()), false)?;

                let res = timeout(SEND_REQUEST_TIMEOUT, res).await.map_err(|_| {
                    io::Error::new(io::ErrorKind::TimedOut, "H2 send_request timed out")
                })??;

                let tunnel_stream = TunnelStream::new(res.into_body(), send_stream);

                let mut tunnel_stream = StreamMonitor {
                    inner: tunnel_stream,
                    read_bytes: self.read_bytes.clone(),
                    written_bytes: self.written_bytes.clone(),
                };

                io::copy_bidirectional(&mut stream, &mut tunnel_stream).await?;
            }

            TunnelConnection::Remote {
                addr,
                proxy_protocol_port,
            } => {
                let mut remote_stream = TcpStream::connect(addr).await?;
                if let Some(port) = proxy_protocol_port {
                    remote_stream
                        .write_all(&proxy_protocol_v2_header(port))
                        .await?;
                }
                io::copy_bidirectional(&mut stream, &mut remote_stream).await?;
            }
        };
        Ok(())
    }
}

impl From<h2::client::SendRequest<Bytes>> for Tunnel {
    fn from(send_request: h2::client::SendRequest<Bytes>) -> Self {
        Tunnel {
            connection: TunnelConnection::Local(send_request),
            read_bytes: Default::default(),
            written_bytes: Default::default(),
        }
    }
}

impl Tunnel {
    /// Builds a tunnel that forwards to a peer server at `addr`. When
    /// `proxy_protocol_port` is set, a reconstructed PROXY protocol v2 header
    /// carrying that port precedes the piped bytes so the peer's TCP proxy can
    /// route the connection.
    fn remote(addr: SocketAddr, proxy_protocol_port: Option<u16>) -> Self {
        Tunnel {
            connection: TunnelConnection::Remote {
                addr,
                proxy_protocol_port,
            },
            read_bytes: Default::default(),
            written_bytes: Default::default(),
        }
    }
}

/// Builds a minimal PROXY protocol v2 header (IPv4 over TCP) advertising
/// `dst_port` as the destination port. It reconstructs exactly the subset of
/// the protocol that `parse_proxy_protocol` reads back, used when forwarding a
/// TCP proxy connection to a peer server: the peer routes by destination port,
/// but this server already consumed the original header while parsing it.
fn proxy_protocol_v2_header(dst_port: u16) -> [u8; 28] {
    let mut header = [0u8; 28];
    // 12-byte signature.
    header[..12].copy_from_slice(b"\x0D\x0A\x0D\x0A\x00\x0D\x0A\x51\x55\x49\x54\x0A");
    // Version 2 + PROXY command.
    header[12] = 0x21;
    // AF_INET over STREAM.
    header[13] = 0x11;
    // Address block length (src addr + dst addr + src port + dst port = 12).
    header[14..16].copy_from_slice(&12u16.to_be_bytes());
    // Destination port; the remaining address fields stay zero, as the peer
    // routes solely by destination port.
    header[26..28].copy_from_slice(&dst_port.to_be_bytes());
    header
}

#[derive(Debug, PartialEq, Eq, Hash, Clone, Copy)]
pub struct TunnelId(Uuid);

impl Default for TunnelId {
    fn default() -> Self {
        Self::new()
    }
}

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

impl PartialEq<str> for TunnelName {
    fn eq(&self, other: &str) -> bool {
        match self {
            TunnelName::Subdomain(subdomain) => subdomain == other,
            TunnelName::PortNumber(port) => other.parse::<u16>() == Ok(*port),
        }
    }
}

impl Display for TunnelName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TunnelName::Subdomain(name) => write!(f, "{}", name),
            TunnelName::PortNumber(num) => write!(f, ":{}", num),
        }
    }
}

struct Registry {
    tunnels: HashMap<TunnelId, (Tunnel, TunnelName)>,
    names: HashMap<TunnelName, TunnelId>,
}

/// A tunnel resolved to live on a peer server, as returned by a metadata query.
#[derive(Debug, Clone, Copy)]
struct RemoteLocation {
    addr: SocketAddr,
    proxy_protocol_port: Option<u16>,
}

/// A cached peer resolution. `location` is `None` for a negative result (the
/// name was not found on any peer), cached briefly so a flood of requests for
/// an unknown name doesn't sweep every peer each time.
struct PeerCacheEntry {
    location: Option<RemoteLocation>,
    expires_at: Instant,
}

pub struct Tunnels<R: PeerResolver> {
    registry: RwLock<Registry>,
    peer_resolver: R,
    peer_cache: RwLock<HashMap<TunnelName, PeerCacheEntry>>,
    http: hyper::Client<hyper::client::HttpConnector>,
    http_proxy_port: u16,
    tcp_proxy_port: u16,
}

impl Default for Tunnels<Vec<SocketAddr>> {
    fn default() -> Self {
        Self::new(vec![])
    }
}

impl<R: PeerResolver> Tunnels<R> {
    pub fn new(peer_resolver: R) -> Self {
        Self::with_peer_proxy_ports(peer_resolver, 5555, 5556)
    }

    pub fn with_peer_proxy_ports(
        peer_resolver: R,
        http_proxy_port: u16,
        tcp_proxy_port: u16,
    ) -> Self {
        Self {
            registry: RwLock::new(Registry {
                tunnels: HashMap::new(),
                names: HashMap::new(),
            }),
            peer_resolver,
            peer_cache: RwLock::new(HashMap::new()),
            http: hyper::Client::new(),
            http_proxy_port,
            tcp_proxy_port,
        }
    }

    pub async fn new_http_tunnel(
        self: &Arc<Self>,
        name: Option<String>,
    ) -> Result<TunnelEntry<R>, RegistryError> {
        let tun_id = TunnelId::new();
        let name = TunnelName::Subdomain(name.unwrap_or_else(|| tun_id.to_string()));

        {
            let mut reg = self.registry.write().unwrap_or_else(|e| e.into_inner());
            if reg.names.contains_key(&name) {
                return Err(RegistryError::SubdomainConflict);
            }
            reg.names.insert(name.clone(), tun_id);
        }

        Ok(TunnelEntry {
            tun_id,
            name,
            registry: self.clone(),
            persisted: false,
        })
    }

    fn reserve_port(
        &self,
        port: Option<u16>,
        tun_id: TunnelId,
    ) -> Result<(TunnelName, u16), RegistryError> {
        let mut reg = self.registry.write().unwrap_or_else(|e| e.into_inner());
        match port {
            Some(port) => {
                let name = TunnelName::PortNumber(port);
                if reg.names.contains_key(&name) {
                    Err(RegistryError::PortConflict(port))
                } else {
                    reg.names.insert(name.clone(), tun_id);
                    Ok((name, port))
                }
            }
            None => {
                for _ in 0..MAX_PORT_RETRIES {
                    let port = rand::thread_rng().gen_range(PORT_RANGE);
                    let name = TunnelName::PortNumber(port);
                    if !reg.names.contains_key(&name) {
                        reg.names.insert(name.clone(), tun_id);
                        return Ok((name, port));
                    }
                }
                Err(RegistryError::PortExhausted)
            }
        }
    }

    pub async fn new_tcp_proxy_tunnel(
        self: &Arc<Self>,
        port: Option<u16>,
    ) -> Result<(TunnelEntry<R>, u16), RegistryError> {
        let tun_id = TunnelId::new();
        let (name, port) = self.reserve_port(port, tun_id)?;

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
    ) -> Result<(TunnelId, u16), RegistryError> {
        let tun_id = TunnelId::new();
        let (_, port) = self.reserve_port(port, tun_id)?;

        Ok((tun_id, port))
    }

    pub async fn tunnel_for_name(&self, name: &TunnelName) -> Option<Tunnel> {
        // 1. Locally-registered tunnel.
        {
            let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
            if let Some(tun_id) = reg.names.get(name) {
                if let Some(tun) = reg.tunnels.get(tun_id).map(|(tun, _)| tun.clone()) {
                    return Some(tun);
                }
            }
        }

        // 2. A recent peer resolution, if still fresh.
        if let Some(location) = self.cached_peer_location(name) {
            return location.map(|loc| Tunnel::remote(loc.addr, loc.proxy_protocol_port));
        }

        // 3. Ask the peers and remember the outcome (positive or negative).
        let location = self.resolve_peer_location(name).await;
        self.cache_peer_location(name.clone(), location);
        location.map(|loc| Tunnel::remote(loc.addr, loc.proxy_protocol_port))
    }

    /// Returns a fresh cached resolution for `name`: `Some(Some(loc))` if it is
    /// known to live on a peer, `Some(None)` if it is known-absent, or `None`
    /// when there is no fresh entry and the peers must be queried.
    fn cached_peer_location(&self, name: &TunnelName) -> Option<Option<RemoteLocation>> {
        let cache = self.peer_cache.read().unwrap_or_else(|e| e.into_inner());
        let entry = cache.get(name)?;
        if entry.expires_at <= Instant::now() {
            return None;
        }
        Some(entry.location)
    }

    fn cache_peer_location(&self, name: TunnelName, location: Option<RemoteLocation>) {
        let ttl = if location.is_some() {
            PEER_CACHE_TTL
        } else {
            PEER_CACHE_NEGATIVE_TTL
        };
        let entry = PeerCacheEntry {
            location,
            expires_at: Instant::now() + ttl,
        };

        let mut cache = self.peer_cache.write().unwrap_or_else(|e| e.into_inner());
        // Bound memory: before inserting a new key into a full cache, drop any
        // entries that have already expired (e.g. a flood of unknown names).
        if cache.len() >= PEER_CACHE_MAX_ENTRIES && !cache.contains_key(&name) {
            let now = Instant::now();
            cache.retain(|_, entry| entry.expires_at > now);
        }
        cache.insert(name, entry);
    }

    async fn resolve_peer_location(&self, name: &TunnelName) -> Option<RemoteLocation> {
        // The lookup key must be the bare name (no `:` prefix that `Display`
        // adds for ports) so the peer's metadata endpoint can match it.
        let key = match name {
            TunnelName::Subdomain(subdomain) => subdomain.clone(),
            TunnelName::PortNumber(port) => port.to_string(),
        };

        for server in self.peer_resolver.resolve().await {
            tracing::trace!("Querying {server} for {name}");
            let query = self
                .http
                .get(Uri::try_from(format!("http://{server}/tunnels/{key}")).unwrap());
            match timeout(PEER_QUERY_TIMEOUT, query).await {
                Ok(Ok(res)) if res.status().is_success() => {
                    let (proxy_port, proxy_protocol_port) = match name {
                        TunnelName::Subdomain(_) => (self.http_proxy_port, None),
                        TunnelName::PortNumber(port) => (self.tcp_proxy_port, Some(*port)),
                    };
                    return Some(RemoteLocation {
                        addr: SocketAddr::new(server.ip(), proxy_port),
                        proxy_protocol_port,
                    });
                }
                Ok(_) => continue,
                Err(_) => {
                    tracing::warn!("Timed out querying peer {server} for {name}");
                    continue;
                }
            }
        }

        None
    }

    pub async fn remove_tunnel(&self, tun_id: TunnelId) {
        let mut reg = self.registry.write().unwrap_or_else(|e| e.into_inner());
        if let Some((_, name)) = reg.tunnels.remove(&tun_id) {
            reg.names.remove(&name);
        }
    }

    pub async fn list_tunnels_metadata(&self) -> Vec<TunnelMetadata> {
        let reg = self.registry.read().unwrap_or_else(|e| e.into_inner());
        reg.tunnels
            .iter()
            .map(|(id, (tunnel, name))| TunnelMetadata {
                id: *id,
                name: name.clone(),
                read_bytes: tunnel.read_bytes.load(Ordering::Relaxed),
                written_bytes: tunnel.written_bytes.load(Ordering::Relaxed),
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelMetadata {
    pub id: TunnelId,
    pub name: TunnelName,
    pub read_bytes: usize,
    pub written_bytes: usize,
}

pub struct TunnelEntry<R: PeerResolver> {
    tun_id: TunnelId,
    name: TunnelName,
    registry: Arc<Tunnels<R>>,
    persisted: bool,
}

impl<R: PeerResolver> TunnelEntry<R> {
    pub fn id(&self) -> TunnelId {
        self.tun_id
    }

    pub fn name(&self) -> &TunnelName {
        &self.name
    }

    pub async fn add_tunnel(&mut self, tunnel: Tunnel) {
        let mut reg = self
            .registry
            .registry
            .write()
            .unwrap_or_else(|e| e.into_inner());
        reg.tunnels.insert(self.tun_id, (tunnel, self.name.clone()));
        self.persisted = true;
    }
}

impl<R: PeerResolver> Drop for TunnelEntry<R> {
    fn drop(&mut self) {
        if !self.persisted {
            let mut reg = self
                .registry
                .registry
                .write()
                .unwrap_or_else(|e| e.into_inner());
            reg.names.remove(&self.name);
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
