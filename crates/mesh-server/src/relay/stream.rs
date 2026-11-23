use std::pin::Pin;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::response::Response;
use futures_util::Stream;

use super::tracker::Tracker;
use crate::registry::InFlightGuard;

/// Keep `guard` alive until `response`'s body has been sent in full, has
/// failed, or has been dropped (for example when the consumer disconnects).
///
/// A long streamed response, such as server-sent events, therefore counts as
/// in flight until its last chunk.
pub fn guard_response(response: Response, guard: InFlightGuard) -> Response {
    watch(response, Some(guard), None)
}

/// Show `response`'s body to `tracker` as it is sent, and drop the tracker,
/// which completes the log row, when the body has ended, has failed, or has
/// been dropped.
pub(super) fn track_response(response: Response, tracker: Tracker) -> Response {
    watch(response, None, Some(tracker))
}

fn watch(response: Response, guard: Option<InFlightGuard>, tracker: Option<Tracker>) -> Response {
    let (parts, body) = response.into_parts();
    let stream = GuardedStream {
        inner: body.into_data_stream(),
        guard,
        tracker,
    };
    Response::from_parts(parts, Body::from_stream(stream))
}

/// A stream that holds a guard and feeds a tracker until it ends.
struct GuardedStream<S> {
    inner: S,
    guard: Option<InFlightGuard>,
    tracker: Option<Tracker>,
}

impl<S, T, E> Stream for GuardedStream<S>
where
    S: Stream<Item = Result<T, E>> + Unpin,
    T: AsRef<[u8]>,
{
    type Item = Result<T, E>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        let poll = Pin::new(&mut this.inner).poll_next(cx);
        match &poll {
            Poll::Ready(Some(Ok(chunk))) => {
                if let Some(tracker) = &mut this.tracker {
                    tracker.observe(chunk.as_ref());
                }
            }
            Poll::Ready(None | Some(Err(_))) => {
                this.guard = None;
                this.tracker = None;
            }
            Poll::Pending => {}
        }
        poll
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use futures_util::stream;

    use super::*;
    use crate::registry::InFlight;

    fn streamed(chunks: Vec<Result<&'static str, std::io::Error>>) -> Response {
        let mut response = Response::new(Body::from_stream(stream::iter(chunks)));
        *response.status_mut() = StatusCode::OK;
        response
    }

    #[tokio::test]
    async fn counts_until_the_body_has_been_read() {
        let counter = InFlight::default();
        let response = guard_response(streamed(vec![Ok("a"), Ok("b")]), counter.start());
        assert_eq!(counter.count(), 1);
        let bytes = to_bytes(response.into_body(), 16).await.unwrap();
        assert_eq!(&bytes[..], b"ab");
        assert_eq!(counter.count(), 0);
    }

    #[tokio::test]
    async fn keeps_the_response_head() {
        let counter = InFlight::default();
        let mut response = streamed(vec![Ok("x")]);
        *response.status_mut() = StatusCode::ACCEPTED;
        response
            .headers_mut()
            .insert("x-kept", "yes".parse().unwrap());
        let response = guard_response(response, counter.start());
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(response.headers()["x-kept"], "yes");
    }

    #[tokio::test]
    async fn stops_counting_when_the_body_is_dropped_early() {
        let counter = InFlight::default();
        let response = guard_response(streamed(vec![Ok("a"), Ok("b")]), counter.start());
        assert_eq!(counter.count(), 1);
        drop(response);
        assert_eq!(counter.count(), 0);
    }

    #[tokio::test]
    async fn stops_counting_when_the_body_fails() {
        let counter = InFlight::default();
        let failing = vec![Ok("a"), Err(std::io::Error::other("reset"))];
        let response = guard_response(streamed(failing), counter.start());
        assert!(to_bytes(response.into_body(), 16).await.is_err());
        assert_eq!(counter.count(), 0);
    }
}
