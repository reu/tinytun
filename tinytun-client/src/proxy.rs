use std::error::Error;

use hyper::{
    client::HttpConnector,
    header::{self, HeaderValue},
    http::uri::Scheme,
    Body, Client, Request, Response, Uri,
};

#[derive(Debug)]
pub struct ReverseProxy {
    http: Client<HttpConnector>,
}

impl ReverseProxy {
    pub fn new() -> Self {
        Self {
            http: hyper::Client::new(),
        }
    }

    pub async fn proxy(
        &self,
        proxy_uri: &Uri,
        mut req: Request<Body>,
    ) -> Result<Response<Body>, Box<dyn Error + Send + Sync>> {
        remove_hop_by_hop_headers(&mut req);

        if let Some(host) = proxy_uri
            .host()
            .and_then(|host| HeaderValue::from_str(host).ok())
        {
            req.headers_mut().insert(header::HOST, host);
        }

        *req.uri_mut() = {
            let mut uri = req.uri().clone().into_parts();
            uri.scheme = Some(Scheme::HTTP);
            uri.authority = proxy_uri.authority().cloned();
            Uri::from_parts(uri)?
        };

        Ok(self.http.request(req).await?)
    }
}

fn remove_hop_by_hop_headers(req: &mut Request<Body>) {
    for key in &[
        header::CONTENT_LENGTH,
        header::TRANSFER_ENCODING,
        header::ACCEPT_ENCODING,
        header::CONTENT_ENCODING,
    ] {
        req.headers_mut().remove(key);
    }
}
