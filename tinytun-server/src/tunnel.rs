use std::{
    error::Error,
    fmt::Display,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use bytes::{Bytes, BytesMut};
use dashmap::DashMap;
use hyper::Request;
use rand::{thread_rng, Rng};
use serde::{Serialize, Serializer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
use tracing::{instrument, trace};

#[derive(Debug, Clone)]
pub struct Tunnel {
    client: h2::client::SendRequest<Bytes>,
    read_bytes: Arc<AtomicUsize>,
    written_bytes: Arc<AtomicUsize>,
}

impl Tunnel {
    #[instrument(skip(self, stream))]
    pub async fn tunnel(&self, stream: TcpStream) -> Result<(), Box<dyn Error + Send + Sync>> {
        let mut sender = self.client.clone().ready().await?;
        let read_bytes = self.read_bytes.clone();
        let written_bytes = self.written_bytes.clone();

        let (res, mut send_stream) = sender.send_request(Request::new(()), false)?;

        let (mut reader, mut writer) = stream.into_split();

        let write = async move {
            loop {
                let mut buf = vec![0; 8 * 1024];
                match reader.read(&mut buf).await? {
                    0 => {
                        trace!("Finishing writing");
                        send_stream.send_data(Bytes::default(), true).unwrap();
                        break;
                    }
                    n => {
                        trace!(bytes = n, "Writing");
                        let buf = BytesMut::from(&buf[0..n]);
                        send_stream.send_data(buf.into(), false)?;
                        written_bytes.fetch_add(n, Ordering::Relaxed);
                    }
                }
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        };

        let read = async move {
            trace!("Sending request");
            let res = res.await?;
            trace!(status = %res.status(), "Upstream response");
            let mut body = res.into_body();
            let mut flow_control = body.flow_control().clone();
            while let Some(chunk) = body.data().await {
                let chunk = chunk?;
                if chunk.is_empty() {
                    break;
                }
                let len = chunk.len();
                trace!(bytes = len, "Reading");
                flow_control.release_capacity(len)?;
                writer.write_all(&chunk).await?;
                read_bytes.fetch_add(len, Ordering::Relaxed);
            }
            trace!("Finishing reading");
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        };

        tokio::try_join!(write, read)?;

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
pub struct TunnelId([u8; 16]);

impl TunnelId {
    pub fn new() -> Self {
        // TODO: use uuid instead
        TunnelId(thread_rng().gen())
    }
}

impl Display for TunnelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for TunnelId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

pub struct Tunnels {
    tunnels: DashMap<TunnelId, (Tunnel, String)>,
    subdomains: DashMap<String, TunnelId>,
}

impl Tunnels {
    pub fn new() -> Self {
        Self {
            tunnels: DashMap::new(),
            subdomains: DashMap::new(),
        }
    }

    pub async fn new_tunnel(
        self: &Arc<Self>,
        subdomain: Option<String>,
    ) -> Result<TunnelEntry, Box<dyn Error + Send + Sync>> {
        let tun_id = TunnelId::new();
        let subdomain = subdomain.unwrap_or_else(|| tun_id.to_string());

        if self.subdomains.contains_key(&subdomain) {
            return Err("Subdomain already in use".into());
        }

        self.subdomains.insert(subdomain.clone(), tun_id);

        Ok(TunnelEntry {
            tun_id,
            subdomain,
            registry: self.clone(),
            persisted: false,
        })
    }

    pub async fn tunnel_for_subdomain(&self, subdomain: &str) -> Option<Tunnel> {
        let tun_id = self.subdomains.get(subdomain)?;
        self.tunnels.get(&tun_id).map(|tun| tun.value().0.clone())
    }

    pub async fn remove_tunnel(&self, tun_id: TunnelId) {
        if let Some((_, (_, subdomain))) = self.tunnels.remove(&tun_id) {
            self.subdomains.remove(&subdomain);
        }
    }

    pub async fn list_tunnels_metadata(&self) -> impl Iterator<Item = TunnelMetadata> + '_ {
        self.tunnels.iter().map(|item| {
            let (tunnel, subdomain) = item.value();
            TunnelMetadata {
                id: *item.key(),
                subdomain: subdomain.clone(),
                read_bytes: tunnel.read_bytes.load(Ordering::Relaxed),
                written_bytes: tunnel.written_bytes.load(Ordering::Relaxed),
            }
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct TunnelMetadata {
    pub id: TunnelId,
    pub subdomain: String,
    pub read_bytes: usize,
    pub written_bytes: usize,
}

pub struct TunnelEntry {
    tun_id: TunnelId,
    subdomain: String,
    registry: Arc<Tunnels>,
    persisted: bool,
}

impl TunnelEntry {
    pub fn id(&self) -> TunnelId {
        self.tun_id
    }

    pub fn subdomain(&self) -> &str {
        self.subdomain.as_str()
    }

    pub async fn add_tunnel(&mut self, tunnel: Tunnel) {
        self.registry
            .tunnels
            .insert(self.tun_id, (tunnel, self.subdomain.clone()));
        self.persisted = true;
    }
}

impl Drop for TunnelEntry {
    fn drop(&mut self) {
        if !self.persisted {
            let subdomain = self.subdomain.clone();
            let tunnels = self.registry.clone();
            tunnels.subdomains.remove(&subdomain);
        }
    }
}
