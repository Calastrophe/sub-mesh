//! The local HTTP server that receives the OAuth redirect.
//!
//! It speaks just enough HTTP/1.1 for a browser's `GET`: one request per
//! connection, a small fixed response, then close.

use std::net::Ipv4Addr;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio::time::timeout;

use super::RedirectParams;
use crate::ProviderError;

/// Longest request head we read. A redirect URL is far shorter.
const MAX_HEAD_BYTES: usize = 8 * 1024;
const MAX_CONNECTIONS: usize = 16;

/// How long one connection may take to send its request or take our reply.
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(10);

const SUCCESS_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
    <title>mesh login</title></head><body style=\"font-family:sans-serif\">\
    <h1>Login complete</h1><p>You can close this tab and return to the terminal.</p>\
    </body></html>";

const FAILURE_PAGE: &str = "<!doctype html><html><head><meta charset=\"utf-8\">\
    <title>mesh login</title></head><body style=\"font-family:sans-serif\">\
    <h1>Login failed</h1><p>Return to the terminal for details.</p></body></html>";

/// Listens on `127.0.0.1` for one OAuth redirect to `path`.
pub(crate) struct CallbackServer {
    listener: TcpListener,
    path: &'static str,
    state: String,
}

impl CallbackServer {
    /// Bind `127.0.0.1:port`. Port 0 picks a free port.
    pub(crate) async fn bind(
        port: u16,
        path: &'static str,
        state: String,
    ) -> Result<Self, ProviderError> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|error| {
                ProviderError::Other(format!(
                    "cannot listen on 127.0.0.1:{port} for the login callback ({error}); \
                     free the port or log in with the manual option"
                ))
            })?;
        Ok(CallbackServer {
            listener,
            path,
            state,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.listener
            .local_addr()
            .map_or(0, |address| address.port())
    }

    /// Wait for a redirect with the right `state` and return its `code`.
    /// Requests with a wrong state, a wrong path, or no code get an error
    /// page and do not end the wait.
    pub(crate) async fn wait_for_code(self, limit: Duration) -> Result<String, ProviderError> {
        let mut connections = JoinSet::new();
        let deadline = tokio::time::sleep(limit);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                () = &mut deadline => return Err(ProviderError::Other(format!(
                    "timed out after {} seconds waiting for the browser to finish the login",
                    limit.as_secs()
                ))),
                finished = connections.join_next(), if !connections.is_empty() => {
                    if let Some(Ok(Some(result))) = finished {
                        return result;
                    }
                }
                accepted = self.listener.accept(), if connections.len() < MAX_CONNECTIONS => {
                    let (stream, _) = accepted?;
                    connections.spawn(serve(stream, self.path, self.state.clone()));
                }
            }
        }
    }
}

/// What a request to the callback server means.
#[derive(Debug, PartialEq, Eq)]
enum Visit {
    NotFound,
    BadRequest,
    /// The provider reported an error for our `state`.
    Denied(&'static str),
    Code(String),
}

async fn serve(
    mut stream: TcpStream,
    path: &'static str,
    state: String,
) -> Option<Result<String, ProviderError>> {
    let Ok(Ok(head)) = timeout(CONNECTION_TIMEOUT, read_head(&mut stream)).await else {
        return None;
    };
    let request = classify(&head, path, &state);
    let (status, page) = match &request {
        Visit::Code(_) => ("200 OK", SUCCESS_PAGE),
        Visit::NotFound => ("404 Not Found", FAILURE_PAGE),
        Visit::BadRequest | Visit::Denied(_) => ("400 Bad Request", FAILURE_PAGE),
    };
    let _ = timeout(CONNECTION_TIMEOUT, respond(&mut stream, status, page)).await;
    match request {
        Visit::Code(code) => Some(Ok(code)),
        Visit::Denied(reason) => Some(Err(ProviderError::Other(format!(
            "login was not completed: {reason}"
        )))),
        Visit::NotFound | Visit::BadRequest => None,
    }
}

/// Read up to the blank line that ends the request head.
async fn read_head(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut head = Vec::new();
    let mut chunk = [0u8; 1024];
    while head.len() < MAX_HEAD_BYTES {
        let remaining = chunk.len().min(MAX_HEAD_BYTES - head.len());
        let read = stream.read(&mut chunk[..remaining]).await?;
        if read == 0 {
            break;
        }
        let search_from = head.len().saturating_sub(3);
        head.extend_from_slice(&chunk[..read]);
        if head[search_from..]
            .windows(4)
            .any(|window| window == b"\r\n\r\n")
        {
            return Ok(head);
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "incomplete or oversized callback request head",
    ))
}

fn classify(head: &[u8], path: &str, state: &str) -> Visit {
    let Ok(text) = std::str::from_utf8(head) else {
        return Visit::BadRequest;
    };
    let mut line = text
        .lines()
        .next()
        .unwrap_or_default()
        .split_ascii_whitespace();
    let (Some("GET"), Some(target), Some("HTTP/1.0" | "HTTP/1.1"), None) =
        (line.next(), line.next(), line.next(), line.next())
    else {
        return Visit::BadRequest;
    };
    let (request_path, query) = target.split_once('?').unwrap_or((target, ""));
    if request_path != path {
        return Visit::NotFound;
    }
    if target.contains('#') || target.bytes().any(|byte| byte.is_ascii_control()) {
        return Visit::BadRequest;
    }

    let Some(RedirectParams {
        code,
        state: got_state,
        error,
    }) = RedirectParams::parse(query)
    else {
        return Visit::BadRequest;
    };
    if got_state.as_deref() != Some(state) {
        return Visit::BadRequest;
    }
    if let Some(reason) = error {
        return Visit::Denied(reason);
    }
    match code.filter(|code| !code.is_empty()) {
        Some(code) => Visit::Code(code),
        None => Visit::BadRequest,
    }
}

async fn respond(stream: &mut TcpStream, status: &str, page: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nCache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    stream.write_all(response.as_bytes()).await?;
    stream.shutdown().await
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn get(port: u16, target: &str) -> String {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        let request = format!("GET {target} HTTP/1.1\r\nHost: localhost\r\n\r\n");
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        response
    }

    #[test]
    fn classifies_requests() {
        let head = |target: &str| format!("GET {target} HTTP/1.1\r\n\r\n").into_bytes();
        let classify = |target: &str| classify(&head(target), "/cb", "s1");
        assert_eq!(
            classify("/cb?code=abc&state=s1"),
            Visit::Code("abc".to_owned())
        );
        assert_eq!(
            classify("/cb?code=a%20b&state=s1"),
            Visit::Code("a b".to_owned())
        );
        assert_eq!(classify("/cb?code=abc&state=other"), Visit::BadRequest);
        assert_eq!(classify("/cb?code=abc"), Visit::BadRequest);
        assert_eq!(
            classify("/cb?code=abc&state=other&state=s1"),
            Visit::BadRequest
        );
        assert_eq!(classify("/nested/../cb?code=abc&state=s1"), Visit::NotFound);
        assert_eq!(classify("/cb?state=s1"), Visit::BadRequest);
        assert_eq!(classify("/favicon.ico"), Visit::NotFound);
        assert_eq!(
            classify("/cb?error=access_denied%3Cb%3E&state=s1"),
            Visit::Denied("authorization failed")
        );
        assert_eq!(classify("/cb?error=access_denied"), Visit::BadRequest);
        assert_eq!(
            super::classify(b"POST /cb HTTP/1.1\r\n\r\n", "/cb", "s1"),
            Visit::BadRequest
        );
        assert_eq!(super::classify(b"garbage", "/cb", "s1"), Visit::BadRequest);
    }

    #[tokio::test]
    async fn returns_the_code_and_ignores_bad_requests() {
        let server = CallbackServer::bind(0, "/cb", "state-1".to_owned())
            .await
            .unwrap();
        let port = server.port();
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(10)));

        assert!(get(port, "/favicon.ico").await.starts_with("HTTP/1.1 404"));
        assert!(
            get(port, "/cb?code=evil&state=wrong")
                .await
                .starts_with("HTTP/1.1 400")
        );
        let ok = get(port, "/cb?code=good&state=state-1").await;
        assert!(ok.starts_with("HTTP/1.1 200"), "{ok}");
        assert!(ok.contains("Login complete"));

        assert_eq!(waiting.await.unwrap().unwrap(), "good");
    }

    #[tokio::test]
    async fn incomplete_and_oversized_heads_do_not_end_the_login() {
        let server = CallbackServer::bind(0, "/cb", "s".to_owned())
            .await
            .unwrap();
        let port = server.port();
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(10)));

        let request_line = "GET /cb?code=bad&state=s HTTP/1.1\r\n";
        let oversized = format!(
            "{request_line}{}",
            "a".repeat(MAX_HEAD_BYTES - request_line.len())
        );
        for request in [request_line, oversized.as_str()] {
            let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .await
                .unwrap();
            stream.write_all(request.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).await.unwrap();
            assert!(response.is_empty());
        }
        assert!(
            get(port, "/cb?code=good&state=s")
                .await
                .starts_with("HTTP/1.1 200")
        );
        assert_eq!(waiting.await.unwrap().unwrap(), "good");
    }

    #[tokio::test]
    async fn reports_a_provider_error() {
        let server = CallbackServer::bind(0, "/cb", "s".to_owned())
            .await
            .unwrap();
        let port = server.port();
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(10)));
        get(port, "/cb?error=access_denied&state=s").await;
        let error = waiting.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("access_denied"), "{error}");
    }

    #[tokio::test]
    async fn times_out() {
        let server = CallbackServer::bind(0, "/cb", "s".to_owned())
            .await
            .unwrap();
        let error = server
            .wait_for_code(Duration::from_millis(50))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("timed out"), "{error}");
    }

    #[tokio::test]
    async fn a_silent_connection_does_not_block_others() {
        let server = CallbackServer::bind(0, "/cb", "s".to_owned())
            .await
            .unwrap();
        let port = server.port();
        let waiting = tokio::spawn(server.wait_for_code(Duration::from_secs(10)));
        let _idle = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
            .await
            .unwrap();
        get(port, "/cb?code=c&state=s").await;
        assert_eq!(waiting.await.unwrap().unwrap(), "c");
    }

    #[tokio::test]
    async fn port_in_use_is_a_clear_error() {
        let first = CallbackServer::bind(0, "/cb", "s".to_owned())
            .await
            .unwrap();
        let error = CallbackServer::bind(first.port(), "/cb", "s".to_owned())
            .await
            .err()
            .expect("second bind fails")
            .to_string();
        assert!(error.contains("cannot listen"), "{error}");
    }
}
