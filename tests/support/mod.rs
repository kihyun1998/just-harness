//! Test doubles shared by the integration tests: a scripted transport and
//! builders for SSE event streams.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use futures_util::stream;
use just_harness::api::{
    ApiError, ClientConfig, ContentBlock, CreateMessageRequest, HttpRequest, HttpResponse,
    MessageParam, Role, Transport,
};
use serde_json::{Value, json};

/// Status, headers and body chunks, or a transport failure.
pub type QueuedResponse = Result<(u16, Vec<(String, String)>, Vec<Bytes>), String>;

/// A transport that answers with queued responses and records each request.
pub struct FakeTransport {
    responses: Mutex<VecDeque<QueuedResponse>>,
    requests: Mutex<Vec<HttpRequest>>,
}

impl FakeTransport {
    pub fn new() -> Self {
        Self {
            responses: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn respond(self, status: u16, chunks: Vec<&str>) -> Self {
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

    pub fn respond_with_header(self, status: u16, header: (&str, &str), chunks: Vec<&str>) -> Self {
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

    pub fn fail(self, message: &str) -> Self {
        self.responses
            .lock()
            .unwrap()
            .push_back(Err(message.to_string()));
        self
    }

    pub fn requests(&self) -> Vec<HttpRequest> {
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

pub fn config() -> ClientConfig {
    let mut config = ClientConfig::new("test-key");
    config.base_url = "https://api.example.test/".into();
    config.initial_retry_delay = Duration::ZERO;
    config
}

pub fn request() -> CreateMessageRequest {
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
pub fn sse(data: Value) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        data["type"].as_str().unwrap(),
        data
    )
}

pub fn message_start(input_tokens: u64) -> String {
    sse(json!({
        "type": "message_start",
        "message": {
            "id": "msg_01", "type": "message", "role": "assistant", "model": "claude-haiku-5-5",
            "content": [], "stop_reason": null, "stop_sequence": null,
            "usage": { "input_tokens": input_tokens, "output_tokens": 1 }
        }
    }))
}

pub fn message_end(stop_reason: &str, output_tokens: u64) -> String {
    sse(json!({
        "type": "message_delta",
        "delta": { "stop_reason": stop_reason, "stop_sequence": null },
        "usage": { "output_tokens": output_tokens }
    })) + &sse(json!({ "type": "message_stop" }))
}

/// A whole reply that calls each `(id, name, input)` tool, then stops with `stop_reason`.
pub fn tool_reply(calls: &[(&str, &str, Value)], stop_reason: &str) -> String {
    let mut out = message_start(10);
    for (index, (id, name, input)) in calls.iter().enumerate() {
        out += &sse(json!({ "type": "content_block_start", "index": index,
            "content_block": { "type": "tool_use", "id": id, "name": name, "input": {} } }));
        out += &sse(json!({ "type": "content_block_delta", "index": index,
            "delta": { "type": "input_json_delta", "partial_json": input.to_string() } }));
        out += &sse(json!({ "type": "content_block_stop", "index": index }));
    }
    out + &message_end(stop_reason, 5)
}

/// A whole reply of one text block that stops with `stop_reason`.
pub fn text_reply(text: &str, stop_reason: &str) -> String {
    message_start(10)
        + &sse(
            json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "" } }),
        )
        + &sse(
            json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "text_delta", "text": text } }),
        )
        + &sse(json!({ "type": "content_block_stop", "index": 0 }))
        + &message_end(stop_reason, 5)
}

/// The JSON body of the `n`th request the transport received.
pub fn sent_body(transport: &FakeTransport, n: usize) -> Value {
    serde_json::from_slice(&transport.requests()[n].body).unwrap()
}

pub fn user(text: &str) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}

/// A transport that plays back the responses `examples/record.rs` recorded
/// under `tests/fixtures/<scenario>/` (`NN.json` and `NN.body`), in order,
/// each body cut into the chunks it arrived in.
pub fn replay(scenario: &str) -> FakeTransport {
    let dir = std::path::Path::new("tests/fixtures").join(scenario);
    let mut metas: Vec<_> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .filter(|p| !p.to_string_lossy().ends_with(".request.json"))
        .collect();
    metas.sort();
    let transport = FakeTransport::new();
    for meta_path in metas {
        let meta: Value =
            serde_json::from_str(&std::fs::read_to_string(&meta_path).unwrap()).unwrap();
        let body = Bytes::from(std::fs::read(meta_path.with_extension("body")).unwrap());
        let mut chunks = Vec::new();
        let mut at = 0;
        for size in meta["chunks"].as_array().unwrap() {
            let size = size.as_u64().unwrap() as usize;
            chunks.push(body.slice(at..at + size));
            at += size;
        }
        assert_eq!(
            at,
            body.len(),
            "{}: chunk sizes do not cover the body",
            meta_path.display()
        );
        let headers = meta["headers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| {
                (
                    h[0].as_str().unwrap().to_string(),
                    h[1].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let status = meta["status"].as_u64().unwrap() as u16;
        transport
            .responses
            .lock()
            .unwrap()
            .push_back(Ok((status, headers, chunks)));
    }
    transport
}
