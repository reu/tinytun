use std::{net::SocketAddr, sync::Arc, time::Duration};

use hyper::{
    server::conn::AddrIncoming,
    service::{make_service_fn, service_fn},
    Body, Request, Response, Server, StatusCode,
};
use tinytun::Tunnel;
use tinytun_server::{start_api, start_http_proxy, start_metadata_api, tunnel::Tunnels};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

struct TestServer {
    api_addr: SocketAddr,
    proxy_addr: SocketAddr,
    metadata_addr: SocketAddr,
}

impl TestServer {
    async fn start() -> Self {
        let tuns = Arc::new(Tunnels::new());
        let base_domain = Arc::new("test.tinytun.local".to_string());

        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let metadata_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();

        let api_addr = api_listener.local_addr().unwrap();
        let proxy_addr = proxy_listener.local_addr().unwrap();
        let metadata_addr = metadata_listener.local_addr().unwrap();

        tokio::spawn(start_api(tuns.clone(), base_domain.clone(), api_listener));
        tokio::spawn(start_http_proxy(tuns.clone(), proxy_listener));
        tokio::spawn(start_metadata_api(tuns.clone(), metadata_listener));

        TestServer {
            api_addr,
            proxy_addr,
            metadata_addr,
        }
    }

    fn server_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.api_addr.port())
    }

    fn proxy_addr(&self) -> SocketAddr {
        self.proxy_addr
    }

    fn metadata_url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{}", self.metadata_addr.port(), path)
    }
}

#[tokio::test]
async fn test_http_tunnel_establishment() {
    let server = TestServer::start().await;

    let tun = Tunnel::builder()
        .server_url(server.server_url())
        .listen()
        .await
        .expect("tunnel should connect");

    let proxy_url = tun.proxy_url();
    assert!(
        proxy_url.contains("test.tinytun.local"),
        "proxy_url should contain base domain, got: {proxy_url}"
    );
}

#[tokio::test]
async fn test_http_tunnel_custom_subdomain() {
    let server = TestServer::start().await;

    let tun = Tunnel::builder()
        .server_url(server.server_url())
        .subdomain("my-custom-app".to_string())
        .listen()
        .await
        .expect("tunnel should connect");

    let proxy_url = tun.proxy_url();
    assert!(
        proxy_url.contains("my-custom-app"),
        "proxy_url should contain custom subdomain, got: {proxy_url}"
    );
}

#[tokio::test]
async fn test_http_proxy_round_trip() {
    let server = TestServer::start().await;

    // Start a local echo HTTP server
    let echo_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let echo_addr = echo_listener.local_addr().unwrap();

    tokio::spawn(async move {
        Server::builder(AddrIncoming::from_listener(echo_listener).unwrap())
            .serve(make_service_fn(|_| async {
                Ok::<_, hyper::Error>(service_fn(|req: Request<Body>| async move {
                    let body = hyper::body::to_bytes(req.into_body()).await.unwrap();
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("x-echo", "true")
                        .body(Body::from(body))
                }))
            }))
            .await
    });

    let mut tun = Tunnel::builder()
        .server_url(server.server_url())
        .subdomain("roundtrip".to_string())
        .listen()
        .await
        .expect("tunnel should connect");

    let proxy_addr = server.proxy_addr();

    // Spawn a task to accept tunnel streams and forward them to the echo server
    let forward_handle = tokio::spawn(async move {
        while let Some(mut remote_stream) = tun.accept().await {
            let echo_addr = echo_addr;
            tokio::spawn(async move {
                let mut local_stream = tokio::net::TcpStream::connect(echo_addr).await.unwrap();
                io::copy_bidirectional(&mut remote_stream, &mut local_stream)
                    .await
                    .ok();
            });
        }
    });

    // Give the tunnel a moment to be fully established
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send an HTTP request through the proxy
    let mut stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
    let request = format!(
        "POST / HTTP/1.1\r\nHost: roundtrip.test.tinytun.local\r\nContent-Length: 11\r\nConnection: close\r\n\r\nhello world"
    );
    stream.write_all(request.as_bytes()).await.unwrap();

    let mut response = Vec::new();
    timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("should not timeout")
        .expect("should read response");

    let response_str = String::from_utf8_lossy(&response);
    assert!(
        response_str.contains("200"),
        "response should contain 200 status, got: {response_str}"
    );
    assert!(
        response_str.contains("x-echo: true"),
        "response should contain echo header, got: {response_str}"
    );
    assert!(
        response_str.contains("hello world"),
        "response should echo back body, got: {response_str}"
    );

    forward_handle.abort();
}

#[tokio::test]
async fn test_metadata_api_lists_tunnel() {
    let server = TestServer::start().await;

    let _tun = Tunnel::builder()
        .server_url(server.server_url())
        .subdomain("meta-test".to_string())
        .listen()
        .await
        .expect("tunnel should connect");

    // Give the tunnel a moment to be registered
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = hyper::Client::new();
    let res = client
        .get(server.metadata_url("/tunnels").parse().unwrap())
        .await
        .expect("metadata request should succeed");

    assert_eq!(res.status(), StatusCode::OK);

    let body = hyper::body::to_bytes(res.into_body()).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);

    assert!(
        body_str.contains("meta-test"),
        "metadata should contain tunnel subdomain, got: {body_str}"
    );
}

#[tokio::test]
async fn test_tunnel_cleanup_after_disconnect() {
    let server = TestServer::start().await;

    let tun = Tunnel::builder()
        .server_url(server.server_url())
        .subdomain("cleanup-test".to_string())
        .listen()
        .await
        .expect("tunnel should connect");

    // Give the tunnel a moment to be registered
    tokio::time::sleep(Duration::from_millis(100)).await;

    let client = hyper::Client::new();

    // Verify it appears in metadata
    let res = client
        .get(server.metadata_url("/tunnels").parse().unwrap())
        .await
        .unwrap();
    let body = hyper::body::to_bytes(res.into_body()).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        body_str.contains("cleanup-test"),
        "tunnel should appear before disconnect, got: {body_str}"
    );

    // Drop the tunnel client to disconnect
    drop(tun);

    // Wait for the server to detect the disconnect and clean up
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Verify it disappears from metadata
    let res = client
        .get(server.metadata_url("/tunnels").parse().unwrap())
        .await
        .unwrap();
    let body = hyper::body::to_bytes(res.into_body()).await.unwrap();
    let body_str = String::from_utf8_lossy(&body);
    assert!(
        !body_str.contains("cleanup-test"),
        "tunnel should disappear after disconnect, got: {body_str}"
    );
}

#[tokio::test]
async fn test_subdomain_conflict() {
    let server = TestServer::start().await;

    let _tun1 = Tunnel::builder()
        .server_url(server.server_url())
        .subdomain("taken".to_string())
        .listen()
        .await
        .expect("first tunnel should connect");

    // Attempt to create a second tunnel with the same subdomain
    let result = Tunnel::builder()
        .server_url(server.server_url())
        .subdomain("taken".to_string())
        .listen()
        .await;

    assert!(
        result.is_err(),
        "second tunnel with same subdomain should fail"
    );
}
