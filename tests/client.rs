use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use just_harness::api::{
    ApiError, BlockKind, Client, ClientConfig, ContentBlock, CreateMessageRequest, HttpRequest,
    HttpResponse, MessageParam, Role, StopReason, Tool, Transport,
};
use serde_json::{Value, json};

/// Status, headers and body chunks, or a transport failure.
type QueuedResponse = Result<(u16, Vec<(String, String)>, Vec<Bytes>), String>;

/// A transport that answers with queued responses and records each request.
struct FakeTransport {
    responses: Mutex<VecDeque<QueuedResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    fn respond(self, status: u16, chunks: Vec<&str>) -> Self {
        let chunks = chunks
            .into_iter()
            .map(|c| Bytes::from(c.to_string()))
            .collect();
        self.responses
            .lock()
            .unwrap()
            .push_back(Ok((status, Vec::new(), chunks)));
        self
    }

    fn respond_with_header(self, status: u16, header: (&str, &str), chunks: Vec<&str>) -> Self {
        let chunks = chunks
            .into_iter()
            .map(|c| Bytes::from(c.to_string()))
            .collect();
        let headers = vec![(header.0.to_string(), header.1.to_string())];
        self.responses
            .lock()
            .unwrap()
            .push_back(Ok((status, headers, chunks)));
        self
    }

    fn fail(self, message: &str) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push_back(Err(message.to_string()));
        self
    }

    fn requests(&self) -> Vec<HttpRequest> {
        self.requests.lock().unwrap().clone()
    }
}

impl Transport for &FakeTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        self.requests.lock().unwrap().push(request);
        let (status, headers, chunks) = self
            .responses
            .lock()
            .unwrap()
            .pop_front()
            .expect("no response queued")
            .map_err(ApiError::Transport)?;
        Ok(HttpResponse {
            status,
            headers,
            body: Box::pin(stream::iter(chunks.into_iter().map(Ok))),
        })
    }
}

fn config() -> ClientConfig {
    let mut config = ClientConfig::new("test-key");
    config.base_url = "https://api.example.test/".into();
    config.initial_retry_delay = Duration::ZERO;
    config
}

fn request() -> CreateMessageRequest {
    CreateMessageRequest::new(
        "claude-haiku-5-5",
        1024,
        vec![MessageParam {
            role: Role::User,
            content: vec![ContentBlock::text("Hi")],
        }],
    )
}

/// One SSE event in the wire format: `event:` line, `data:` line, blank line.
fn sse(data: Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        data["type"].as_str().unwrap(),
        data
    )
}

fn message_start(input_tokens: u64) -> String {
    sse(json!({
        "type": "message_start",
        "message": {
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-haiku-5-5",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": { "input_tokens": input_tokens, "output_tokens": 1 }
        }
    }))
}

fn message_end(stop_reason: &str, output_tokens: u64) -> String {
    sse(json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason, "stop_sequence": null },
        "usage": { "output_tokens": output_tokens }
    })) + &sse(json!({ "type": "message_stop" }))
}

#[tokio::test]
async fn assembles_a_streamed_text_reply() {
    let stream = [
        message_start(25),
        sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": "Hello" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": ", world" } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        message_end("end_turn", 15),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(message.id, "msg_01");
    assert_eq!(message.content.len(), 1);
    assert_eq!(message.content[0].kind(), BlockKind::Text("Hello, world"));
    assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(message.usage.input_tokens, 25);
    assert_eq!(message.usage.output_tokens, 15);
}

#[tokio::test]
async fn parses_tool_input_split_across_deltas() {
    let stream = [
        message_start(10),
        sse(json!({ "type": "content_block_start", "index": 0,
            "content_block": { "type": "tool_use", "id": "toolu_01", "name": "read_file", "input": {} } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"pa" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "th\": \"src/" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "main.rs\"}" } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        message_end("tool_use", 30),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(
        message.content[0].kind(),
        BlockKind::ToolUse {
            id: "toolu_01",
            name: "read_file",
            input: &json!({ "path": "src/main.rs" })
        }
    );
    assert_eq!(message.stop_reason, Some(StopReason::ToolUse));
}

#[tokio::test]
async fn keeps_thinking_with_its_signature_and_unknown_blocks_for_the_next_request() {
    let stream = [
        message_start(10),
        sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "thinking_delta", "thinking": "Need the file." } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "c2lnLTE=" } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        sse(json!({ "type": "content_block_start", "index": 1, "content_block": { "type": "future_block", "payload": { "x": 1 } } })),
        sse(json!({ "type": "content_block_stop", "index": 1 })),
        message_end("end_turn", 5),
    ]
    .concat();
    let transport = FakeTransport::new()
        .respond(200, vec![&stream])
        .respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();
    let mut next = request();
    next.messages.push(message.to_param());
    client.send(&next, |_| {}).await.unwrap();

    let sent: Value = serde_json::from_slice(&transport.requests()[1].body).unwrap();
    assert_eq!(
        sent["messages"][1],
        json!({
            "role": "assistant",
            "content": [
                { "type": "thinking", "thinking": "Need the file.", "signature": "c2lnLTE=" },
                { "type": "future_block", "payload": { "x": 1 } }
            ]
        })
    );
}

#[tokio::test]
async fn sends_a_streaming_request_with_version_and_auth_headers() {
    let stream = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    client.send(&request(), |_| {}).await.unwrap();

    let sent = &transport.requests()[0];
    assert_eq!(sent.url, "https://api.example.test/v1/messages");
    let header = |name: &str| {
        sent.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(header("anthropic-version"), Some("2026-10-21"));
    assert_eq!(header("authorization"), Some("Bearer test-key"));
    assert_eq!(header("content-type"), Some("application/json"));
    let body: Value = serde_json::from_slice(&sent.body).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "claude-haiku-5-5",
            "max_tokens": 1024,
            "messages": [{ "role": "user", "content": [{ "type": "text", "text": "Hi" }] }],
            "stream": true
        })
    );
}

#[tokio::test]
async fn skips_pings_and_hands_every_other_event_to_the_callback() {
    let stream = [
        message_start(1),
        "event: ping\ndata: {\"type\": \"ping\"}\n\n".to_string(),
        message_end("end_turn", 1),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let mut seen = Vec::new();
    client
        .send(&request(), |e| {
            seen.push(e["type"].as_str().unwrap().to_string())
        })
        .await
        .unwrap();

    assert_eq!(seen, ["message_start", "message_delta", "message_stop"]);
}

#[tokio::test]
async fn an_error_event_mid_stream_fails_without_a_retry() {
    let stream = [
        message_start(1),
        "event: error\ndata: {\"type\": \"error\", \"error\": {\"type\": \"overloaded_error\", \"message\": \"Overloaded\"}}\n\n".to_string(),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let err = client.send(&request(), |_| {}).await.unwrap_err();

    assert_eq!(
        err,
        ApiError::Stream {
            body: json!({ "type": "error", "error": { "type": "overloaded_error", "message": "Overloaded" } })
        }
    );
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn retries_429_and_connection_failures_then_succeeds() {
    let ok = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new()
        .respond(429, vec!["{\"type\":\"error\"}"])
        .fail("connection reset")
        .respond(200, vec![&ok]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(message.id, "msg_01");
    assert_eq!(transport.requests().len(), 3);
}

#[tokio::test]
async fn gives_up_after_max_retries_on_a_5xx() {
    let transport = FakeTransport::new()
        .respond(500, vec!["{\"n\":1}"])
        .respond(529, vec!["{\"n\":2}"])
        .respond(503, vec!["{\"n\":3}"]);
    let client = Client::with_transport(config(), &transport);

    let err = client.send(&request(), |_| {}).await.unwrap_err();

    assert_eq!(
        err,
        ApiError::Status {
            status: 503,
            body: json!({ "n": 3 })
        }
    );
    assert_eq!(transport.requests().len(), 3);
}

#[tokio::test]
async fn does_not_retry_a_400() {
    let transport = FakeTransport::new().respond(
        400,
        vec!["{\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\"}}"],
    );
    let client = Client::with_transport(config(), &transport);

    let err = client.send(&request(), |_| {}).await.unwrap_err();

    assert_eq!(
        err,
        ApiError::Status {
            status: 400,
            body: json!({ "type": "error", "error": { "type": "invalid_request_error" } })
        }
    );
    assert!(!err.is_retryable());
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn rejects_events_before_message_start_and_a_stream_cut_before_message_stop() {
    let early = sse(
        json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
    );
    let cut = message_start(1);
    let transport = FakeTransport::new()
        .respond(200, vec![&early])
        .respond(200, vec![&cut]);
    let client = Client::with_transport(config(), &transport);

    let early_err = client.send(&request(), |_| {}).await.unwrap_err();
    let cut_err = client.send(&request(), |_| {}).await.unwrap_err();

    assert!(
        matches!(&early_err, ApiError::Protocol(m) if m.contains("before `message_start`")),
        "{early_err:?}"
    );
    assert!(
        matches!(&cut_err, ApiError::Transport(m) if m.contains("before `message_stop`")),
        "{cut_err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn waits_as_long_as_retry_after_says() {
    let ok = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new()
        .respond_with_header(429, ("retry-after", "3"), vec!["{}"])
        .respond(200, vec![&ok]);
    let client = Client::with_transport(config(), &transport);

    let started = tokio::time::Instant::now();
    client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(started.elapsed(), Duration::from_secs(3));
}

#[tokio::test]
async fn keeps_a_message_whose_tool_input_was_cut_by_max_tokens() {
    let stream = [
        message_start(10),
        sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "Reading it." } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        sse(json!({ "type": "content_block_start", "index": 1,
            "content_block": { "type": "tool_use", "id": "toolu_02", "name": "read_file", "input": {} } })),
        sse(json!({ "type": "content_block_delta", "index": 1, "delta": { "type": "input_json_delta", "partial_json": "{\"path\": \"src/ma" } })),
        sse(json!({ "type": "content_block_stop", "index": 1 })),
        message_end("max_tokens", 8),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(message.stop_reason, Some(StopReason::MaxTokens));
    assert_eq!(message.content[0].kind(), BlockKind::Text("Reading it."));
    assert_eq!(
        message.content[1].kind(),
        BlockKind::ToolUse {
            id: "toolu_02",
            name: "read_file",
            input: &json!({})
        }
    );
    assert_eq!(message.usage.output_tokens, 8);
}

#[tokio::test]
async fn rejects_tool_input_that_is_not_json_when_not_cut_off() {
    let stream = [
        message_start(10),
        sse(json!({ "type": "content_block_start", "index": 0,
            "content_block": { "type": "tool_use", "id": "toolu_03", "name": "read_file", "input": {} } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "input_json_delta", "partial_json": "{\"path\": " } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        message_end("tool_use", 8),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let err = client.send(&request(), |_| {}).await.unwrap_err();

    assert!(
        matches!(&err, ApiError::Protocol(m) if m.contains("tool input for block 0")),
        "{err:?}"
    );
}

#[tokio::test]
async fn keeps_cache_usage_and_refusal_details_from_message_delta() {
    let stream = [
        message_start(10),
        sse(json!({
            "type": "message_delta",
            "delta": { "stop_reason": "refusal", "stop_sequence": null,
                       "stop_details": { "type": "refusal", "category": "cyber", "explanation": "x" } },
            "usage": { "output_tokens": 3, "cache_read_input_tokens": 2048, "cache_creation_input_tokens": 0 }
        })),
        sse(json!({ "type": "message_stop" })),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(message.stop_reason, Some(StopReason::Refusal));
    assert_eq!(
        message.stop_details,
        Some(json!({ "type": "refusal", "category": "cyber", "explanation": "x" }))
    );
    assert_eq!(message.usage.cache_read_input_tokens, Some(2048));
    assert_eq!(message.usage.cache_creation_input_tokens, Some(0));
    assert_eq!(message.usage.input_tokens, 10);
}

#[tokio::test]
async fn sends_cache_control_on_system_blocks_and_tools() {
    let stream = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);
    let mut req = request();
    req.system = vec![ContentBlock::text("You are a coding agent.").with_cache_control()];
    req.tools.push(Tool {
        name: "read_file".into(),
        description: "Read a file.".into(),
        input_schema: json!({ "type": "object" }),
        cache_control: Some(json!({ "type": "ephemeral" })),
    });

    client.send(&req, |_| {}).await.unwrap();

    let body: Value = serde_json::from_slice(&transport.requests()[0].body).unwrap();
    assert_eq!(
        body["system"],
        json!([{ "type": "text", "text": "You are a coding agent.", "cache_control": { "type": "ephemeral" } }])
    );
    assert_eq!(
        body["tools"],
        json!([{ "name": "read_file", "description": "Read a file.", "input_schema": { "type": "object" },
                 "cache_control": { "type": "ephemeral" } }])
    );
}

#[tokio::test]
async fn does_not_retry_a_404() {
    let transport = FakeTransport::new().respond(
        404,
        vec!["{\"type\":\"error\",\"error\":{\"type\":\"not_found_error\"}}"],
    );
    let client = Client::with_transport(config(), &transport);

    let err = client.send(&request(), |_| {}).await.unwrap_err();

    assert!(
        matches!(err, ApiError::Status { status: 404, .. }),
        "{err:?}"
    );
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn obeys_x_should_retry_over_the_status_code() {
    let ok = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new()
        .respond_with_header(400, ("x-should-retry", "true"), vec!["{}"])
        .respond(200, vec![&ok])
        .respond_with_header(503, ("x-should-retry", "false"), vec!["{}"]);
    let client = Client::with_transport(config(), &transport);

    client.send(&request(), |_| {}).await.unwrap();
    let err = client.send(&request(), |_| {}).await.unwrap_err();

    assert!(
        matches!(err, ApiError::Status { status: 503, .. }),
        "{err:?}"
    );
    assert_eq!(transport.requests().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn prefers_retry_after_ms_and_honours_long_retry_after() {
    let ok = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new()
        .respond_with_header(429, ("retry-after-ms", "1500"), vec!["{}"])
        .respond(200, vec![&ok])
        .respond_with_header(429, ("retry-after", "120"), vec!["{}"])
        .respond(200, vec![&ok]);
    let client = Client::with_transport(config(), &transport);

    let started = tokio::time::Instant::now();
    client.send(&request(), |_| {}).await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_millis(1500));

    let started = tokio::time::Instant::now();
    client.send(&request(), |_| {}).await.unwrap();
    assert_eq!(started.elapsed(), Duration::from_secs(120));
}

#[tokio::test(start_paused = true)]
async fn a_zero_retry_after_falls_back_to_backoff() {
    let ok = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new()
        .respond_with_header(429, ("retry-after", "0"), vec!["{}"])
        .respond(200, vec![&ok]);
    let mut config = config();
    config.initial_retry_delay = Duration::from_millis(500);
    let client = Client::with_transport(config, &transport);

    let started = tokio::time::Instant::now();
    client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(started.elapsed(), Duration::from_millis(500));
}

#[test]
fn debug_output_hides_the_api_key() {
    let config = ClientConfig::new("sk-ant-secret");
    let request = HttpRequest {
        url: "https://api.example.test/v1/messages".into(),
        headers: vec![
            ("authorization".into(), "Bearer sk-ant-secret".into()),
            ("x-api-key".into(), "sk-ant-secret".into()),
            ("anthropic-version".into(), "2026-10-21".into()),
        ],
        body: b"{}".to_vec(),
    };

    let printed = format!("{config:?} {request:?}");

    assert!(!printed.contains("sk-ant-secret"), "{printed}");
    assert!(printed.contains("[redacted]"));
    assert!(printed.contains("2026-10-21"));
}

#[tokio::test]
async fn skips_events_that_carry_no_event_name() {
    let stream = [
        message_start(1),
        "data: not json\n\n".to_string(),
        message_end("end_turn", 1),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
}

#[tokio::test]
async fn sends_extra_fields_and_tool_results_as_given() {
    let stream = [message_start(1), message_end("end_turn", 1)].concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);
    let mut req = request();
    req.extra
        .insert("output_config".into(), json!({ "effort": "low" }));
    req.messages.push(MessageParam {
        role: Role::User,
        content: vec![
            ContentBlock::tool_result("toolu_01", "fn main() {}", false),
            ContentBlock::tool_result("toolu_02", "no such file", true),
        ],
    });

    client.send(&req, |_| {}).await.unwrap();

    let body: Value = serde_json::from_slice(&transport.requests()[0].body).unwrap();
    assert_eq!(body["output_config"], json!({ "effort": "low" }));
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            { "type": "tool_result", "tool_use_id": "toolu_01", "content": "fn main() {}" },
            { "type": "tool_result", "tool_use_id": "toolu_02", "content": "no such file", "is_error": true }
        ])
    );
}

#[tokio::test]
async fn appends_citations_to_their_text_block() {
    let first = json!({ "type": "char_location", "cited_text": "a", "document_index": 0 });
    let second = json!({ "type": "char_location", "cited_text": "b", "document_index": 0 });
    let stream = [
        message_start(1),
        sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "citations_delta", "citation": first } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "citations_delta", "citation": second } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        message_end("end_turn", 1),
    ]
    .concat();
    let transport = FakeTransport::new().respond(200, vec![&stream]);
    let client = Client::with_transport(config(), &transport);

    let message = client.send(&request(), |_| {}).await.unwrap();

    assert_eq!(message.content[0].0["citations"], json!([first, second]));
}
