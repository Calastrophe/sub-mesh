//! Stand-ins for what the server meets in a deployment behind a reverse
//! proxy: the proxy itself, and tailscaled for direct callers.

use std::convert::Infallible;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use http::header::{CONNECTION, UPGRADE};
use http::{HeaderName, HeaderValue, Request, Response, StatusCode};
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket};

/// The header the test proxy names the caller in.
pub const PROXY_HEADER: &str = "x-webauth-user";

/// Start an HTTP/1.1 reverse proxy on loopback, as Caddy with
/// `tailscale_auth` behaves: it drops any `x-webauth-user` the client sent,
/// adds one per entry of `logins` (none, one, or an ambiguous several),
/// keeps `Host`, connects to `upstream` from the address `from`, and passes
/// any `Upgrade` through. Returns its address.
pub async fn start_proxy(
    upstream: SocketAddr,
    from: IpAddr,
    logins: &'static [&'static str],
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let service = service_fn(move |request| forward(request, upstream, from, logins));
            tokio::spawn(async move {
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await;
            });
        }
    });
    addr
}

async fn forward(
    mut request: Request<Incoming>,
    upstream: SocketAddr,
    from: IpAddr,
    logins: &'static [&'static str],
) -> Result<Response<Body>, Infallible> {
    let wants_upgrade = request.headers().contains_key(UPGRADE);
    let client_side = wants_upgrade.then(|| hyper::upgrade::on(&mut request));
    let header = HeaderName::from_static(PROXY_HEADER);
    request.headers_mut().remove(&header);
    for login in logins {
        request
            .headers_mut()
            .append(&header, HeaderValue::from_static(login));
    }
    if !wants_upgrade {
        request.headers_mut().remove(CONNECTION);
    }

    let socket = TcpSocket::new_v4().unwrap();
    socket.bind(SocketAddr::new(from, 0)).unwrap();
    let Ok(stream) = socket.connect(upstream).await else {
        let mut response = Response::new(Body::from("upstream is down"));
        *response.status_mut() = StatusCode::BAD_GATEWAY;
        return Ok(response);
    };
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .unwrap();
    tokio::spawn(connection.with_upgrades());
    let mut response = sender.send_request(request).await.unwrap();
    if response.status() == StatusCode::SWITCHING_PROTOCOLS {
        let upstream_side = hyper::upgrade::on(&mut response);
        let client_side = client_side.expect("upgrade answered without being asked");
        tokio::spawn(async move {
            let (Ok(client), Ok(upstream)) = tokio::join!(client_side, upstream_side) else {
                return;
            };
            let _ = tokio::io::copy_bidirectional(
                &mut TokioIo::new(client),
                &mut TokioIo::new(upstream),
            )
            .await;
        });
    }
    Ok(response.map(Body::new))
}

/// A tailscaled on a Unix socket that answers every whois with `login` on
/// the node `laptop`, and counts the whois calls.
#[cfg(unix)]
pub fn fake_tailscaled(login: &'static str) -> (PathBuf, Arc<AtomicUsize>) {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let path = std::env::temp_dir().join(format!(
        "mesh-server-ts-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_file(&path);
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let counter = counter.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = stream.read(&mut buf).await.unwrap();
                    if n == 0 {
                        return;
                    }
                    request.extend_from_slice(&buf[..n]);
                }
                assert!(request.starts_with(b"GET /localapi/v0/whois?"));
                counter.fetch_add(1, Ordering::SeqCst);
                let body = format!(
                    r#"{{"Node":{{"Name":"laptop.ts.net.","ComputedName":"laptop"}},"UserProfile":{{"LoginName":"{login}"}}}}"#
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
            });
        }
    });
    (path, calls)
}
