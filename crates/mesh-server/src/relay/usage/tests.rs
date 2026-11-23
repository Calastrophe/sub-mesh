use super::*;

fn tally(provider: Provider, events: &[&str]) -> Completion {
    let mut tally = Tally::new(provider);
    for event in events {
        tally.event(event);
    }
    tally.completion()
}

fn usage(input: u64, cache_read: u64, cache_write: u64, output: u64, reasoning: u64) -> TokenUsage {
    TokenUsage {
        input,
        cache_read,
        cache_write,
        output,
        reasoning,
    }
}

const CODEX_COMPLETED: &str = r#"{"type":"response.completed","response":{"id":"r1","usage":{"input_tokens":100,"input_tokens_details":{"cached_tokens":40},"output_tokens":20,"output_tokens_details":{"reasoning_tokens":5},"total_tokens":120}}}"#;

#[test]
fn codex_usage_comes_from_the_completed_event() {
    let found = tally(
        Provider::Codex,
        &[
            r#"{"type":"response.created","response":{"id":"r1","usage":null}}"#,
            r#"{"type":"response.output_text.delta","delta":"hi"}"#,
            CODEX_COMPLETED,
        ],
    );
    assert_eq!(found.responses, 1);
    assert_eq!(found.usage, Some(usage(100, 40, 0, 20, 5)));
    assert_eq!(found.error, None);
}

#[test]
fn codex_frames_of_several_responses_add_up() {
    let second = r#"{"type":"response.completed","response":{"usage":{"input_tokens":7,"output_tokens":3}}}"#;
    let incomplete = r#"{"type":"response.incomplete","response":{"usage":{"input_tokens":1,"input_tokens_details":{"cached_tokens":1},"output_tokens":2,"output_tokens_details":{"reasoning_tokens":2}}}}"#;
    let found = tally(Provider::Codex, &[CODEX_COMPLETED, second, incomplete]);
    assert_eq!(found.responses, 3);
    assert_eq!(found.usage, Some(usage(108, 41, 0, 25, 7)));
}

#[test]
fn a_codex_response_without_usage_does_not_count() {
    let found = tally(
        Provider::Codex,
        &[r#"{"type":"response.completed","response":{"id":"r1","usage":null}}"#],
    );
    assert_eq!(found.responses, 0);
    assert_eq!(found.usage, None);
}

#[test]
fn codex_failures_give_an_error_and_keep_their_usage() {
    let failed = r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"The model is overloaded"},"usage":{"input_tokens":4,"output_tokens":0}}}"#;
    let found = tally(Provider::Codex, &[failed]);
    assert_eq!(found.error.as_deref(), Some("The model is overloaded"));
    assert_eq!(found.usage, Some(usage(4, 0, 0, 0, 0)));

    let frame = r#"{"type":"error","status":429,"error":{"type":"usage_limit_reached","message":"The usage limit has been reached"},"headers":{}}"#;
    let found = tally(Provider::Codex, &[frame]);
    assert_eq!(
        found.error.as_deref(),
        Some("The usage limit has been reached")
    );
    assert_eq!(found.responses, 0);

    let flat = r#"{"type": "error","code":"x","message":"flat message"}"#;
    assert_eq!(
        tally(Provider::Codex, &[flat]).error.as_deref(),
        Some("flat message")
    );
}

#[test]
fn the_first_error_stays() {
    let first = r#"{"type":"error","error":{"message":"first"}}"#;
    let second = r#"{"type":"error","error":{"message":"second"}}"#;
    assert_eq!(
        tally(Provider::Codex, &[first, second]).error.as_deref(),
        Some("first")
    );
}

#[test]
fn claude_usage_adds_the_cache_to_the_input_and_takes_the_last_output() {
    let found = tally(
        Provider::Claude,
        &[
            r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":10,"cache_creation_input_tokens":20,"cache_read_input_tokens":30,"output_tokens":1}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":15}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    );
    assert_eq!(found.responses, 1);
    assert_eq!(found.usage, Some(usage(60, 30, 20, 15, 0)));
}

#[test]
fn a_claude_stream_that_stops_early_counts_for_nothing() {
    let found = tally(
        Provider::Claude,
        &[r#"{"type":"message_start","message":{"usage":{"input_tokens":10,"output_tokens":1}}}"#],
    );
    assert_eq!(found.responses, 0);
    assert_eq!(found.usage, None);
}

#[test]
fn claude_stream_errors_are_found() {
    let overloaded =
        r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#;
    assert_eq!(
        tally(Provider::Claude, &[overloaded]).error.as_deref(),
        Some("Overloaded")
    );
}

#[test]
fn text_that_names_usage_inside_a_string_is_not_parsed_as_usage() {
    let delta = r#"{"type":"response.output_text.delta","delta":"the \"usage\" key"}"#;
    assert!(!delta.contains(r#""usage""#));
    assert_eq!(tally(Provider::Codex, &[delta]).responses, 0);
}

#[test]
fn unreadable_messages_are_ignored() {
    let found = tally(
        Provider::Codex,
        &[
            r#"{"type":"response.completed","usage"#,
            "[DONE] usage",
            r#"{"type":"response.completed","response":{"usage":"many"}}"#,
        ],
    );
    assert_eq!(found, Completion::default());
}

#[test]
fn a_claude_document_has_its_usage_at_the_end() {
    let mut tally = Tally::new(Provider::Claude);
    tally.document(
        br#"{"id":"m","type":"message","content":[{"type":"text","text":"a \"usage\": {} b"}],"stop_reason":"end_turn","usage":{"input_tokens":3,"cache_creation_input_tokens":0,"cache_read_input_tokens":7,"output_tokens":4,"service_tier":"standard"}}"#,
    );
    let found = tally.completion();
    assert_eq!(found.responses, 1);
    assert_eq!(found.usage, Some(usage(10, 7, 0, 4, 0)));
}

#[test]
fn a_codex_document_has_its_usage_near_the_end() {
    let mut tally = Tally::new(Provider::Codex);
    tally.document(
        br#"{"id":"r","output":[],"usage":{"input_tokens":9,"input_tokens_details":{"cached_tokens":2},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":1}},"user":null,"metadata":{}}"#,
    );
    assert_eq!(tally.completion().usage, Some(usage(9, 2, 0, 5, 1)));
}

#[test]
fn a_document_without_usage_counts_for_nothing() {
    for body in [
        &br#"{"data":[{"id":"gpt"}]}"#[..],
        br#"{"usage":null}"#,
        br#"{"usage":{"input_tokens":"x"}}"#,
        b"",
    ] {
        let mut tally = Tally::new(Provider::Claude);
        tally.document(body);
        assert_eq!(tally.completion(), Completion::default(), "{body:?}");
    }
}

#[test]
fn error_messages_come_from_the_json_the_providers_send() {
    let message = |body: &str| error_message(body.as_bytes());
    assert_eq!(
        message(r#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens is too large"}}"#)
            .as_deref(),
        Some("max_tokens is too large")
    );
    assert_eq!(
        message(r#"{"error":{"message":"Unsupported model","type":"invalid_request_error","code":"model_not_found"}}"#)
            .as_deref(),
        Some("Unsupported model")
    );
    assert_eq!(
        message(r#"{"detail":"Unsupported model"}"#).as_deref(),
        Some("Unsupported model")
    );
    assert_eq!(message(r#"{"error":"plain"}"#).as_deref(), Some("plain"));
}

#[test]
fn other_error_bodies_are_their_own_message() {
    assert_eq!(
        error_message(b"  upstream rate limit\n").as_deref(),
        Some("upstream rate limit")
    );
    assert_eq!(error_message(b"").as_deref(), None);
    assert_eq!(error_message(b" \n").as_deref(), None);
    assert_eq!(
        error_message(br#"{"error":{"message":"cut off "#).as_deref(),
        Some(r#"{"error":{"message":"cut off"#)
    );
}

#[test]
fn long_messages_are_clipped() {
    let long = "x".repeat(1000);
    let message =
        error_message(format!(r#"{{"error":{{"message":"{long}"}}}}"#).as_bytes()).unwrap();
    assert_eq!(message.chars().count(), MAX_ERROR_CHARS);
    assert!(message.ends_with('…'));

    let multibyte = "é".repeat(500);
    let message = error_message(multibyte.as_bytes()).unwrap();
    assert_eq!(message.chars().count(), MAX_ERROR_CHARS);
}
