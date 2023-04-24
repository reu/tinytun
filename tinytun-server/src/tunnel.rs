use std::{error::Error, fmt::Display, sync::Arc};

use dashmap::DashMap;
use hyper::{client::conn::SendRequest, Body, Request, Response};
use rand::{thread_rng, Rng};
use tokio::sync::Mutex;

#[derive(Debug, Clone)]
pub struct Tunnel {
    client: Arc<Mutex<SendRequest<Body>>>,
}

impl Tunnel {
    pub async fn request(
        &self,
        req: Request<Body>,
    ) -> Result<Response<Body>, Box<dyn Error + Send + Sync>> {
        let mut client = self.client.lock().await;
        Ok(client.send_request(req).await?)
    }
}

impl From<SendRequest<Body>> for Tunnel {
    fn from(send_request: SendRequest<Body>) -> Self {
        Tunnel {
            client: Arc::new(Mutex::new(send_request)),
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
            tokio::spawn(async move {
                tunnels.subdomains.remove(&subdomain);
            });
        }
    }
}
