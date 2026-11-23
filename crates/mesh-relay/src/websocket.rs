use std::time::Duration;

use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use bytes::Bytes;
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use http::header::{CONNECTION, CONTENT_LENGTH, HOST, SEC_WEBSOCKET_PROTOCOL, UPGRADE};
use http::request::Parts;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::CloseFrame as UpstreamCloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message as UpstreamMessage};
use url::Url;

use crate::Error;
use crate::headers::{self, MeshHeaders};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// True when `headers` ask for a WebSocket upgrade.
pub fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers::has_token(headers, UPGRADE, "websocket")
        && headers::has_token(headers, CONNECTION, "upgrade")
}

/// Detect an HTTP/1.1 WebSocket upgrade or an HTTP/2 extended WebSocket
/// `CONNECT` (RFC 8441).
pub fn is_websocket_request(parts: &Parts) -> bool {
    if parts.method == Method::CONNECT {
        return parts
            .extensions
            .get::<hyper::ext::Protocol>()
            .is_some_and(|protocol| protocol.as_str().eq_ignore_ascii_case("websocket"));
    }
    is_websocket_upgrade(&parts.headers)
}

/// Request headers that the WebSocket client generates itself.
fn is_handshake_header(name: &HeaderName) -> bool {
    name == HOST
        || name == CONNECTION
        || name == UPGRADE
        || name == CONTENT_LENGTH
        || (name.as_str().starts_with("sec-websocket-") && name != SEC_WEBSOCKET_PROTOCOL)
}

/// Prepare caller headers for a WebSocket handshake: drops the headers the
/// client generates itself (see [`UpstreamSocket::connect`]) and joins
/// several `sec-websocket-protocol` offers into one value.
#[allow(clippy::result_large_err)]
pub(crate) fn handshake_headers(headers: &HeaderMap) -> Result<HeaderMap, Error> {
    let mut wanted = headers::copy_headers(headers, MeshHeaders::Keep, |name| {
        !is_handshake_header(name)
    });
    let offers = wanted.get_all(SEC_WEBSOCKET_PROTOCOL);
    if offers.iter().nth(1).is_some() {
        let offers = offers
            .iter()
            .map(HeaderValue::to_str)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|err| Error::WebSocket(err.to_string()))?;
        let value = HeaderValue::from_str(&offers.join(", "))
            .map_err(|err| Error::WebSocket(err.to_string()))?;
        wanted.insert(SEC_WEBSOCKET_PROTOCOL, value);
    }
    Ok(wanted)
}

/// A client WebSocket over any transport.
pub(super) trait Socket:
    Stream<Item = Result<UpstreamMessage, tungstenite::Error>>
    + Sink<UpstreamMessage, Error = tungstenite::Error>
    + Send
    + Unpin
{
}

impl<T> Socket for T where
    T: Stream<Item = Result<UpstreamMessage, tungstenite::Error>>
        + Sink<UpstreamMessage, Error = tungstenite::Error>
        + Send
        + Unpin
{
}

type UpstreamStream = Box<dyn Socket>;

/// An open upstream WebSocket, connected before the consumer side is
/// upgraded so that a refusal can be returned as a normal HTTP response.
///
/// The transport is either a direct TCP or TLS connection
/// ([`UpstreamSocket::connect`]) or a tunnel stream
/// ([`crate::tunnel::Client::connect_websocket`]).
pub struct UpstreamSocket {
    stream: UpstreamStream,
    response_headers: HeaderMap,
}

impl UpstreamSocket {
    /// Connect over `ws://` or `wss://`. Strip hop-by-hop and generated
    /// handshake headers, keeping subprotocol offers. An HTTP refusal
    /// becomes [`Error::Rejected`].
    #[allow(clippy::result_large_err)]
    pub async fn connect(url: Url, headers: HeaderMap) -> Result<UpstreamSocket, Error> {
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let mut request = url
            .as_str()
            .into_client_request()
            .map_err(|err| Error::WebSocket(err.to_string()))?;
        request.headers_mut().extend(handshake_headers(&headers)?);

        let result =
            tokio::time::timeout(HANDSHAKE_TIMEOUT, tokio_tungstenite::connect_async(request))
                .await
                .map_err(|_| Error::WebSocket("upstream WebSocket handshake timed out".into()))?;
        match result {
            Ok((stream, response)) => {
                Ok(UpstreamSocket::new(stream, response.into_parts().0.headers))
            }
            Err(tungstenite::Error::Http(response)) => {
                let (parts, body) = response.into_parts();
                Err(Error::from_refusal(
                    parts,
                    Bytes::from(body.unwrap_or_default()),
                ))
            }
            Err(err) => Err(Error::WebSocket(err.to_string())),
        }
    }

    pub(crate) fn new(stream: impl Socket + 'static, response_headers: HeaderMap) -> Self {
        UpstreamSocket {
            stream: Box::new(stream),
            response_headers,
        }
    }

    /// Headers of the upstream handshake response (`101` over HTTP/1.1,
    /// `200` over a tunnel).
    pub fn response_headers(&self) -> &HeaderMap {
        &self.response_headers
    }

    /// Send one message upstream before the bridge starts, such as a
    /// client message that was read to choose this socket. Pings, pongs and
    /// closes are not sent.
    #[allow(clippy::result_large_err)]
    pub async fn send(&mut self, message: ws::Message) -> Result<(), Error> {
        let Some(message) = client_to_upstream(message).filter(|message| !message.is_close())
        else {
            return Ok(());
        };
        self.stream
            .send(message)
            .await
            .map_err(|err| Error::WebSocket(err.to_string()))
    }

    /// Upgrade the consumer connection and relay data messages both ways until
    /// either side closes, passing text messages through `hooks`. Each hop
    /// handles its own ping and pong frames. Append `extra` headers to the
    /// handshake response.
    ///
    /// If upstream chose a subprotocol and the consumer offered it, the
    /// consumer gets the same one.
    pub fn bridge(
        self,
        upgrade: WebSocketUpgrade,
        extra: HeaderMap,
        hooks: FrameHooks,
    ) -> Response {
        let upgrade = match self
            .response_headers
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok())
        {
            Some(protocol) => upgrade.protocols([protocol.trim().to_owned()]),
            None => upgrade,
        };
        let mut response =
            upgrade.on_upgrade(move |socket| async move { self.relay(socket, hooks).await });
        for (name, value) in &extra {
            response.headers_mut().append(name.clone(), value.clone());
        }
        response
    }

    /// Relay data messages both ways between an already upgraded consumer
    /// socket and this one until either side closes, passing text messages
    /// through `hooks`. Resolves once both sides are closed.
    pub async fn relay(self, client: WebSocket, hooks: FrameHooks) {
        pump(client, self.stream, hooks).await;
    }
}

/// What a client hook does with one text message from the client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientText {
    /// Send it upstream unchanged.
    Forward,
    /// Keep it from upstream and send this text back to the client instead.
    /// The connection stays open.
    Answer(String),
}

type ClientHook = Box<dyn Fn(&str) -> ClientText + Send + Sync>;
type UpstreamHook = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Optional checks on the text messages of a bridge. With a client hook,
/// binary messages from the client are dropped, so that nothing reaches
/// upstream unchecked. Close frames bypass the hooks, and each hop handles
/// its own pings and pongs. Hooks run on every text message, so they should
/// reject most messages with a cheap test before parsing.
#[derive(Default)]
pub struct FrameHooks {
    client: Option<ClientHook>,
    upstream: Option<UpstreamHook>,
}

impl FrameHooks {
    /// Decide for each text message from the client whether it goes upstream.
    #[must_use]
    pub fn on_client_text(
        mut self,
        hook: impl Fn(&str) -> ClientText + Send + Sync + 'static,
    ) -> Self {
        self.client = Some(Box::new(hook));
        self
    }

    /// Rewrite text messages from upstream: `Some` replaces the message
    /// that reaches the client, and `None` keeps it.
    #[must_use]
    pub fn on_upstream_text(
        mut self,
        hook: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.upstream = Some(Box::new(hook));
        self
    }

    fn client_text(&self, message: &ws::Message) -> ClientText {
        match (&self.client, message) {
            (Some(hook), ws::Message::Text(text)) => hook(text.as_str()),
            _ => ClientText::Forward,
        }
    }

    fn upstream_text(&self, message: UpstreamMessage) -> UpstreamMessage {
        match (&self.upstream, &message) {
            (Some(hook), UpstreamMessage::Text(text)) => match hook(text.as_str()) {
                Some(text) => UpstreamMessage::Text(text.into()),
                None => message,
            },
            _ => message,
        }
    }
}

const CLOSE_TIMEOUT: Duration = Duration::from_secs(5);

/// Forward messages both ways until either side ends, then close both.
///
/// The client sink is shared, because a client hook can answer the client
/// while upstream messages are relayed to it.
async fn pump(client: WebSocket, upstream: UpstreamStream, hooks: FrameHooks) {
    let (client_tx, mut client_rx) = client.split();
    let mut client_tx = Mutex::new(client_tx);
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    let is_client_close = |message: &ws::Message| matches!(message, ws::Message::Close(_));
    let to_upstream = async {
        while let Some(Ok(message)) = client_rx.next().await {
            if hooks.client.is_some() && matches!(message, ws::Message::Binary(_)) {
                continue;
            }
            if let ClientText::Answer(answer) = hooks.client_text(&message) {
                let answer = ws::Message::Text(answer.into());
                if client_tx.lock().await.send(answer).await.is_err() {
                    break;
                }
                continue;
            }
            let Some(message) = client_to_upstream(message) else {
                continue;
            };
            if message.is_close() {
                return message;
            }
            if upstream_tx.send(message).await.is_err() {
                break;
            }
        }
        UpstreamMessage::Close(None)
    };
    let to_client = async {
        while let Some(Ok(message)) = upstream_rx.next().await {
            let Some(message) = upstream_to_client(hooks.upstream_text(message)) else {
                continue;
            };
            if is_client_close(&message) {
                return message;
            }
            if client_tx.lock().await.send(message).await.is_err() {
                break;
            }
        }
        ws::Message::Close(None)
    };
    tokio::select! {
        close = to_upstream => {
            finish_close(
                client_tx.into_inner().close(), &mut upstream_tx, &mut upstream_rx, close,
                UpstreamMessage::is_close,
            ).await;
        }
        close = to_client => {
            finish_close(
                upstream_tx.close(), client_tx.get_mut(), &mut client_rx, close,
                is_client_close,
            ).await;
        }
    }
}

async fn finish_close<Message, ReadError>(
    close_source: impl Future,
    peer_tx: &mut (impl Sink<Message> + Unpin),
    peer_rx: &mut (impl Stream<Item = Result<Message, ReadError>> + Unpin),
    close: Message,
    is_close: impl Fn(&Message) -> bool,
) {
    let close_peer = async {
        if peer_tx.send(close).await.is_ok() {
            while let Some(Ok(message)) = peer_rx.next().await {
                if is_close(&message) {
                    break;
                }
            }
        }
    };
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, async {
        let _ = tokio::join!(close_source, close_peer);
    })
    .await;
}

fn client_to_upstream(message: ws::Message) -> Option<UpstreamMessage> {
    Some(match message {
        ws::Message::Text(text) => UpstreamMessage::Text(text.as_str().into()),
        ws::Message::Binary(data) => UpstreamMessage::Binary(data),
        ws::Message::Ping(_) | ws::Message::Pong(_) => return None,
        ws::Message::Close(frame) => {
            UpstreamMessage::Close(frame.map(|frame| UpstreamCloseFrame {
                code: CloseCode::from(frame.code),
                reason: frame.reason.as_str().into(),
            }))
        }
    })
}

fn upstream_to_client(message: UpstreamMessage) -> Option<ws::Message> {
    Some(match message {
        UpstreamMessage::Text(text) => ws::Message::Text(text.as_str().into()),
        UpstreamMessage::Binary(data) => ws::Message::Binary(data),
        UpstreamMessage::Ping(_) | UpstreamMessage::Pong(_) => return None,
        UpstreamMessage::Close(frame) => ws::Message::Close(frame.map(|frame| ws::CloseFrame {
            code: u16::from(frame.code),
            reason: frame.reason.as_str().into(),
        })),
        UpstreamMessage::Frame(_) => return None,
    })
}
