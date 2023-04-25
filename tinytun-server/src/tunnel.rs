use std::{error::Error, fmt::Display, sync::Arc};

use bytes::Bytes;
use dashmap::DashMap;
use hyper::{body::HttpBody, header, Body, HeaderMap, Request, Response};
use rand::{thread_rng, Rng};

#[derive(Debug, Clone)]
pub struct Tunnel {
    client: h2::client::SendRequest<Bytes>,
}

impl Tunnel {
    pub async fn request(
        &self,
        req: Request<Body>,
    ) -> Result<Response<Body>, Box<dyn Error + Send + Sync>> {
        let mut sender = self.client.clone().ready().await?;
        let (mut req, mut body) = req.into_parts();
        remove_hop_by_hop_headers(&mut req.headers);
        let (res, mut send_stream) = sender.send_request(Request::from_parts(req, ()), false)?;

        tokio::spawn(async move {
            while let Some(chunk) = body.data().await {
                send_stream.send_data(chunk?, false)?;
            }
            send_stream.send_data(Bytes::default(), true)?;
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });

        let res = res.await?;
        let (mut res, mut proxy_body) = res.into_parts();
        remove_hop_by_hop_headers(&mut res.headers);

        let (mut out, res_body) = Body::channel();

        tokio::spawn(async move {
            while let Some(chunk) = proxy_body.data().await {
                out.send_data(chunk?).await?;
            }
            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });

        Ok(Response::from_parts(res, res_body))
    }
}

impl From<h2::client::SendRequest<Bytes>> for Tunnel {
    fn from(send_request: h2::client::SendRequest<Bytes>) -> Self {
        Tunnel {
            client: send_request,
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

fn remove_hop_by_hop_headers(headers: &mut HeaderMap) {
    for key in &[
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
        header::ACCEPT_ENCODING,
        header::CONTENT_ENCODING,
    ] {
        headers.remove(key);
    }
}
