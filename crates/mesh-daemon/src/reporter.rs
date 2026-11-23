//! Sends the daemon's state to the server.

use std::time::Duration;

use axum::body::Body;
use http::header::{CONTENT_TYPE, HOST};
use http::{Method, Request};
use hyper_util::rt::TokioIo;
use mesh_core::protocol::REPORT_PATH;
use tokio::net::TcpStream;
use url::Position;

use crate::peer;
use crate::state::AppState;

/// Wait after a change before reporting, so bursts become one report.
const DEBOUNCE: Duration = Duration::from_secs(1);
const REPORT_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn run(state: &AppState) {
    loop {
        let result = tokio::time::timeout(REPORT_TIMEOUT, send(state))
            .await
            .unwrap_or_else(|_| Err("report timed out".to_owned()));
        if let Err(message) = &result {
            tracing::warn!(server = %state.settings.server_url, "report failed: {message}");
        }
        state.record_report(result);
        tokio::select! {
            () = tokio::time::sleep(state.settings.report_interval) => {}
            () = state.wait_changed() => tokio::time::sleep(DEBOUNCE).await,
        }
    }
}

async fn send(state: &AppState) -> Result<(), String> {
    let server = &state.settings.server;
    let host = &server[Position::BeforeHost..Position::AfterPort];
    let port = server
        .port_or_known_default()
        .expect("validated HTTP server URL");
    let address = format!(
        "{}:{port}",
        server.host_str().expect("validated server host")
    );
    let stream = TcpStream::connect(address)
        .await
        .map_err(|err| err.to_string())?;
    peer::verify_server(state, stream.peer_addr().map_err(|err| err.to_string())?).await?;

    state.refresh_owner().await;
    let body = serde_json::to_vec(&state.daemon_report()).map_err(|err| err.to_string())?;
    let request = Request::builder()
        .method(Method::POST)
        .uri(REPORT_PATH)
        .header(HOST, host)
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(body))
        .map_err(|err| err.to_string())?;
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|err| err.to_string())?;
    let response = sender.send_request(request);
    tokio::pin!(response);
    let response = tokio::select! {
        result = &mut response => result,
        _ = connection => response.await,
    }
    .map_err(|err| err.to_string())?;
    let status = response.status();
    if status.is_success() {
        Ok(())
    } else {
        Err(format!("server answered {status}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tunnel::testing::daemon_state;
    use tokio::io::AsyncReadExt;
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn sends_no_report_bytes_before_peer_is_trusted() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = daemon_state("report-untrusted", &url, "", "", false);
        let (result, received) = tokio::time::timeout(REPORT_TIMEOUT, async {
            tokio::join!(send(&state), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut data = Vec::new();
                stream.read_to_end(&mut data).await.unwrap();
                data
            })
        })
        .await
        .unwrap();
        assert!(result.is_err());
        assert!(received.is_empty());
        let _ = std::fs::remove_dir_all(&state.settings.data_dir);
    }

    #[tokio::test]
    async fn sends_report_to_an_allowed_loopback_peer() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(REPORT_PATH))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;
        let state = daemon_state("report-trusted", &server.uri(), "", "", true);
        send(&state).await.unwrap();
        server.verify().await;
        let _ = std::fs::remove_dir_all(&state.settings.data_dir);
    }
}
