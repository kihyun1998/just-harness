//! Builds one [`Message`] from the stream's events.

use std::collections::HashMap;

use serde_json::Value;

use super::error::ApiError;
use super::types::{ContentBlock, Message, StopReason, Usage};

/// Applies stream events in order. Tool input JSON text is buffered per block
/// index and parsed in [`finish`](Self::finish). Usage on `message_delta`
/// replaces the value from `message_start`.
#[derive(Debug, Default)]
pub struct MessageAccumulator {
    message: Option<Message>,
    json_bufs: HashMap<usize, String>,
    stopped: bool,
}

impl MessageAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn apply(&mut self, event: &Value) -> Result<(), ApiError> {
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "message_start" {
            self.message = Some(parse_message_start(event)?);
            return Ok(());
        }
        let Some(message) = self.message.as_mut() else {
            return Err(ApiError::Protocol(format!(
                "`{kind}` arrived before `message_start`"
            )));
        };
        match kind {
            "content_block_start" => {
                let block = event
                    .get("content_block")
                    .cloned()
                    .ok_or_else(|| protocol(event, "missing `content_block`"))?;
                message.content.push(ContentBlock(block));
            }
            "content_block_delta" => {
                let index = block_index(event)?;
                let block = &mut message
                    .content
                    .get_mut(index)
                    .ok_or_else(|| protocol(event, "delta for a block that never started"))?
                    .0;
                let delta = event
                    .get("delta")
                    .ok_or_else(|| protocol(event, "missing `delta`"))?;
                apply_delta(block, delta, self.json_bufs.entry(index).or_default());
            }
            "message_delta" => {
                if let Some(delta) = event.get("delta") {
                    message.stop_reason = delta
                        .get("stop_reason")
                        .and_then(|v| serde_json::from_value::<Option<StopReason>>(v.clone()).ok())
                        .flatten();
                    message.stop_sequence = delta
                        .get("stop_sequence")
                        .and_then(Value::as_str)
                        .map(String::from);
                    message.stop_details =
                        delta.get("stop_details").filter(|v| !v.is_null()).cloned();
                }
                if let Some(usage) = event.get("usage") {
                    merge_usage(&mut message.usage, usage);
                }
            }
            "message_stop" => self.stopped = true,
            _ => {}
        }
        Ok(())
    }

    /// The assembled message. Tool input that does not parse is an error,
    /// except when the message stopped at `max_tokens`: then the block keeps
    /// the input it started with.
    pub fn finish(self) -> Result<Message, ApiError> {
        let mut message = self
            .message
            .ok_or_else(|| ApiError::Protocol("stream had no `message_start`".into()))?;
        if !self.stopped {
            return Err(ApiError::Transport(
                "stream ended before `message_stop`".into(),
            ));
        }
        let truncated = message.stop_reason == Some(StopReason::MaxTokens);
        for (index, buf) in self.json_bufs {
            if buf.is_empty() {
                continue;
            }
            match serde_json::from_str::<Value>(&buf) {
                Ok(input) => {
                    if let Some(block) = message.content.get_mut(index) {
                        block.0["input"] = input;
                    }
                }
                Err(_) if truncated => {}
                Err(e) => {
                    return Err(ApiError::Protocol(format!(
                        "tool input for block {index} is not JSON: {e}: {buf}"
                    )));
                }
            }
        }
        Ok(message)
    }
}

fn parse_message_start(event: &Value) -> Result<Message, ApiError> {
    let m = event
        .get("message")
        .ok_or_else(|| protocol(event, "missing `message`"))?;
    let str_field = |key: &str| m.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let content = m
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| blocks.iter().cloned().map(ContentBlock).collect())
        .unwrap_or_default();
    let usage = m
        .get("usage")
        .map(|u| serde_json::from_value(u.clone()))
        .transpose()
        .map_err(|e| ApiError::Protocol(format!("bad usage in `message_start`: {e}")))?
        .unwrap_or_default();
    Ok(Message {
        id: str_field("id"),
        model: str_field("model"),
        content,
        stop_reason: None,
        stop_sequence: None,
        stop_details: None,
        usage,
    })
}

fn apply_delta(block: &mut Value, delta: &Value, json_buf: &mut String) {
    let str_field = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or("");
    let append = |block: &mut Value, key: &str, more: &str| match block.get_mut(key) {
        Some(Value::String(text)) => text.push_str(more),
        _ => block[key] = Value::String(more.to_string()),
    };
    match str_field("type") {
        "text_delta" => append(block, "text", str_field("text")),
        "thinking_delta" => append(block, "thinking", str_field("thinking")),
        "signature_delta" => block["signature"] = Value::String(str_field("signature").to_string()),
        "input_json_delta" => json_buf.push_str(str_field("partial_json")),
        "citations_delta" => {
            if let Some(citation) = delta.get("citation") {
                match block.get_mut("citations").and_then(Value::as_array_mut) {
                    Some(list) => list.push(citation.clone()),
                    None => block["citations"] = Value::Array(vec![citation.clone()]),
                }
            }
        }
        _ => {}
    }
}

fn merge_usage(usage: &mut Usage, delta: &Value) {
    let field = |key: &str| delta.get(key).and_then(Value::as_u64);
    if let Some(v) = field("output_tokens") {
        usage.output_tokens = v;
    }
    if let Some(v) = field("input_tokens") {
        usage.input_tokens = v;
    }
    if let Some(v) = field("cache_creation_input_tokens") {
        usage.cache_creation_input_tokens = Some(v);
    }
    if let Some(v) = field("cache_read_input_tokens") {
        usage.cache_read_input_tokens = Some(v);
    }
}

fn block_index(event: &Value) -> Result<usize, ApiError> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .map(|i| i as usize)
        .ok_or_else(|| protocol(event, "missing `index`"))
}

fn protocol(event: &Value, what: &str) -> ApiError {
    ApiError::Protocol(format!("{what} in {event}"))
}
