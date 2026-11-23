use axum::http::HeaderValue;

use super::*;

fn head(content_type: Option<&'static str>) -> HeaderMap {
    let mut headers = HeaderMap::new();
    if let Some(content_type) = content_type {
        headers.insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    }
    headers
}

fn read(mut scan: Scan, chunks: &[&[u8]]) -> Completion {
    for chunk in chunks {
        scan.observe(chunk);
    }
    scan.completion()
}

const CODEX_STREAM: &str = concat!(
    "event: response.created\r\n",
    "data: {\"type\":\"response.created\",\"response\":{\"usage\":null}}\r\n\r\n",
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"hi\"}\n\n",
    ": keep-alive\n",
    "event: response.completed\n",
    "data:{\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}}\n\n",
);

fn event_stream(provider: Provider, path: &str) -> Scan {
    Scan::choose(
        provider,
        path,
        StatusCode::OK,
        &head(Some("text/event-stream; charset=utf-8")),
    )
}

#[test]
fn events_are_read_whatever_the_chunking() {
    let bytes = CODEX_STREAM.as_bytes();
    for size in [1, 2, 7, 64, bytes.len()] {
        let chunks: Vec<&[u8]> = bytes.chunks(size).collect();
        let found = read(event_stream(Provider::Codex, "responses"), &chunks);
        assert_eq!(found.responses, 1, "chunks of {size}");
        let usage = found.usage.unwrap();
        assert_eq!((usage.input, usage.output), (9, 4), "chunks of {size}");
    }
}

#[test]
fn a_last_event_without_a_line_break_still_counts() {
    let text = CODEX_STREAM.trim_end();
    let found = read(
        event_stream(Provider::Codex, "responses/compact"),
        &[text.as_bytes()],
    );
    assert_eq!(found.responses, 1);
}

#[test]
fn a_line_over_the_limit_is_skipped_and_the_next_is_read() {
    let mut huge = b"data: {\"type\":\"x\",\"pad\":\"".to_vec();
    huge.resize(MAX_LINE_BYTES + 10, b'a');
    huge.extend_from_slice(b"\"}\n");
    let mut scan = event_stream(Provider::Codex, "responses");
    for chunk in huge.chunks(10_000) {
        scan.observe(chunk);
    }
    scan.observe(CODEX_STREAM.as_bytes());
    assert_eq!(scan.completion().responses, 1);
}

#[test]
fn the_line_buffer_stays_small_between_lines() {
    let mut lines = DataLines::default();
    let mut seen = 0;
    let mut big = b"data: ".to_vec();
    big.resize(MAX_LINE_BYTES / 2, b'a');
    big.push(b'\n');
    lines.push(&big, &mut |_| seen += 1);
    assert_eq!(seen, 1);
    assert!(lines.line.capacity() <= RETAINED_LINE_BYTES);
}

#[test]
fn only_data_lines_are_events() {
    let mut lines = DataLines::default();
    let mut seen = Vec::new();
    lines.push(
        b"event: a\ndata: one\n: comment\r\ndata:two\r\nid: 3\n\ndata:  three\nretry: 5\ndata: last",
        &mut |data| seen.push(data.to_owned()),
    );
    lines.finish(&mut |data| seen.push(data.to_owned()));
    assert_eq!(seen, ["one", "two", " three", "last"]);
}

#[test]
fn a_document_keeps_only_its_tail() {
    let mut scan = Scan::choose(
        Provider::Claude,
        "v1/messages",
        StatusCode::OK,
        &head(Some("application/json")),
    );
    let filler = vec![b' '; 5 * TAIL_BYTES];
    let body = br#"{"content":[],"usage":{"input_tokens":2,"output_tokens":3}}"#;
    scan.observe(&filler);
    scan.observe(&body[..20]);
    scan.observe(&body[20..]);
    let Scan::Document { tail, .. } = &scan else {
        panic!("not a document scan");
    };
    assert!(tail.bytes.len() <= 2 * TAIL_BYTES);
    let found = scan.completion();
    assert_eq!(found.responses, 1);
    assert_eq!(
        found.usage.map(|usage| (usage.input, usage.output)),
        Some((2, 3))
    );
}

#[test]
fn an_error_body_gives_its_message_from_the_first_bytes() {
    let mut scan = Scan::choose(
        Provider::Claude,
        "v1/messages",
        StatusCode::BAD_REQUEST,
        &head(Some("application/json")),
    );
    scan.observe(br#"{"type":"error","error":{"type":"invalid_request_error","#);
    scan.observe(br#""message":"max_tokens is too large"}}"#);
    scan.observe(&vec![b' '; 3 * FAILURE_BYTES]);
    let Scan::Failure(head) = &scan else {
        panic!("not a failure scan");
    };
    assert_eq!(head.len(), FAILURE_BYTES);
    assert_eq!(
        scan.completion().error.as_deref(),
        Some("max_tokens is too large")
    );
}

#[test]
fn only_model_endpoints_are_scanned_for_usage() {
    let stream = head(Some("text/event-stream"));
    let json = head(Some("application/json"));
    let kind = |provider, path: &str, status, headers: &HeaderMap| match Scan::choose(
        provider, path, status, headers,
    ) {
        Scan::Nothing => "nothing",
        Scan::Failure(_) => "failure",
        Scan::Sniff(_) => "sniff",
        Scan::Events { .. } => "events",
        Scan::Document { .. } => "document",
    };
    let ok = StatusCode::OK;
    assert_eq!(kind(Provider::Codex, "responses", ok, &stream), "events");
    assert_eq!(kind(Provider::Codex, "responses", ok, &json), "document");
    assert_eq!(
        kind(Provider::Codex, "responses/compact", ok, &json),
        "document"
    );
    assert_eq!(kind(Provider::Claude, "v1/messages", ok, &stream), "events");
    assert_eq!(kind(Provider::Claude, "v1/messages", ok, &json), "document");

    assert_eq!(kind(Provider::Codex, "models", ok, &json), "nothing");
    assert_eq!(kind(Provider::Claude, "v1/models", ok, &json), "nothing");
    assert_eq!(
        kind(Provider::Claude, "v1/messages/count_tokens", ok, &json),
        "nothing"
    );
    assert_eq!(kind(Provider::Claude, "responses", ok, &stream), "nothing");
    assert_eq!(kind(Provider::Codex, "responses", ok, &head(None)), "sniff");
    assert_eq!(
        kind(Provider::Claude, "v1/messages", ok, &head(None)),
        "sniff"
    );
    assert_eq!(kind(Provider::Codex, "models", ok, &head(None)), "nothing");
    assert_eq!(
        kind(
            Provider::Codex,
            "responses",
            StatusCode::NO_CONTENT,
            &head(None)
        ),
        "sniff",
        "any success is sniffed"
    );
    assert_eq!(
        kind(Provider::Codex, "responses", ok, &head(Some("text/plain"))),
        "nothing"
    );
    assert_eq!(
        kind(Provider::Codex, "responses", StatusCode::FOUND, &stream),
        "nothing"
    );
}

#[test]
fn errors_are_read_on_any_path_and_compressed_bodies_never() {
    let json = head(Some("application/json"));
    let kind = |path: &str, status, headers: &HeaderMap| {
        matches!(
            Scan::choose(Provider::Codex, path, status, headers),
            Scan::Failure(_)
        )
    };
    assert!(kind("models", StatusCode::NOT_FOUND, &json));
    assert!(kind("responses", StatusCode::BAD_GATEWAY, &head(None)));
    assert!(!kind("responses", StatusCode::OK, &json));

    let mut gzip = json.clone();
    gzip.insert(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
    assert!(!kind("responses", StatusCode::BAD_REQUEST, &gzip));
    gzip.insert(CONTENT_ENCODING, HeaderValue::from_static("identity"));
    assert!(kind("responses", StatusCode::BAD_REQUEST, &gzip));
}

fn unlabeled(provider: Provider, path: &str) -> Scan {
    Scan::choose(provider, path, StatusCode::OK, &head(None))
}

#[test]
fn an_event_stream_without_a_content_type_is_read_whatever_the_chunking() {
    let bytes = CODEX_STREAM.as_bytes();
    for size in [1, 2, 3, 7, 64, bytes.len()] {
        let chunks: Vec<&[u8]> = bytes.chunks(size).collect();
        let found = read(unlabeled(Provider::Codex, "responses"), &chunks);
        assert_eq!(found.responses, 1, "chunks of {size}");
        let usage = found.usage.unwrap();
        assert_eq!((usage.input, usage.output), (9, 4), "chunks of {size}");
    }
}

#[test]
fn a_document_without_a_content_type_is_read() {
    let body = br#"{"content":[],"usage":{"input_tokens":2,"output_tokens":3}}"#;
    for size in [1, 5, body.len()] {
        let chunks: Vec<&[u8]> = body.chunks(size).collect();
        let found = read(unlabeled(Provider::Claude, "v1/messages"), &chunks);
        assert_eq!(found.responses, 1, "chunks of {size}");
        assert_eq!(
            found.usage.map(|usage| (usage.input, usage.output)),
            Some((2, 3)),
            "chunks of {size}"
        );
    }
}

#[test]
fn the_first_bytes_decide_what_an_unlabeled_body_is() {
    let kind = |chunks: &[&[u8]]| {
        let mut scan = unlabeled(Provider::Codex, "responses");
        for chunk in chunks {
            scan.observe(chunk);
        }
        match scan {
            Scan::Sniff(_) => "undecided",
            Scan::Nothing => "nothing",
            Scan::Events { .. } => "events",
            Scan::Document { .. } => "document",
            Scan::Failure(_) => "failure",
        }
    };
    assert_eq!(kind(&[]), "undecided");
    assert_eq!(kind(&[b"  \r\n\t"]), "undecided");
    assert_eq!(kind(&[b"eve"]), "undecided");
    assert_eq!(kind(&[b"eve", b"nt:"]), "events");
    assert_eq!(kind(&[b"\n\ndata:"]), "events");
    assert_eq!(kind(&[b" : keep-alive"]), "events");
    assert_eq!(kind(&[b"\n {"]), "document");
    assert_eq!(kind(&[b"event"]), "undecided");
    assert_eq!(kind(&[b"<html>"]), "nothing");
    assert_eq!(kind(&[b"events"]), "nothing");
    assert_eq!(kind(&[b"da", b"x"]), "nothing");
    assert_eq!(kind(&[b"[1]"]), "nothing");
}

#[test]
fn an_undecided_body_reports_only_its_duration() {
    let found = read(unlabeled(Provider::Codex, "responses"), &[b"  ", b"eve"]);
    assert_eq!(found, Completion::default());
    let found = read(unlabeled(Provider::Codex, "responses"), &[b"<html>"]);
    assert_eq!(found, Completion::default());
}

#[test]
fn a_sniffed_stream_loses_no_bytes_that_decided_it() {
    let text =
        "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":1}}}\n";
    let found = read(
        unlabeled(Provider::Codex, "responses"),
        &[b"\n", &text.as_bytes()[..3], &text.as_bytes()[3..]],
    );
    assert_eq!(found.responses, 1);
    assert_eq!(found.usage.map(|usage| usage.input), Some(1));
}
