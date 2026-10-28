//! Request and response shapes of `POST /v1/messages`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    User,
    Assistant,
}

/// One content block, kept whole as the JSON the API sent or will receive.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentBlock(pub Value);

/// The typed view of a [`ContentBlock`].
#[derive(Debug, Clone, PartialEq)]
pub enum BlockKind<'a> {
    Text(&'a str),
    Thinking(&'a str),
    ToolUse {
        id: &'a str,
        name: &'a str,
        input: &'a Value,
    },
    Other(&'a str),
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        Self(json!({ "type": "text", "text": text.into() }))
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        let mut block = json!({
            "type": "tool_result",
            "tool_use_id": tool_use_id.into(),
            "content": content.into(),
        });
        if is_error {
            block["is_error"] = Value::Bool(true);
        }
        Self(block)
    }

    /// Marks this block as the end of a cacheable prefix.
    pub fn with_cache_control(mut self) -> Self {
        self.0["cache_control"] = json!({ "type": "ephemeral" });
        self
    }

    pub fn block_type(&self) -> &str {
        self.0.get("type").and_then(Value::as_str).unwrap_or("")
    }

    pub fn kind(&self) -> BlockKind<'_> {
        let str_field = |key: &str| self.0.get(key).and_then(Value::as_str).unwrap_or("");
        match self.block_type() {
            "text" => BlockKind::Text(str_field("text")),
            "thinking" => BlockKind::Thinking(str_field("thinking")),
            "tool_use" => BlockKind::ToolUse {
                id: str_field("id"),
                name: str_field("name"),
                input: self.0.get("input").unwrap_or(&Value::Null),
            },
            other => BlockKind::Other(other),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MessageParam {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CreateMessageRequest {
    pub model: String,
    pub max_tokens: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub system: Vec<ContentBlock>,
    pub messages: Vec<MessageParam>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<Tool>,
    /// Further top-level request fields, sent as given.
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl CreateMessageRequest {
    pub fn new(model: impl Into<String>, max_tokens: u32, messages: Vec<MessageParam>) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            system: Vec::new(),
            messages,
            tools: Vec::new(),
            extra: Map::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    StopSequence,
    ToolUse,
    PauseTurn,
    Refusal,
    ModelContextWindowExceeded,
    #[serde(untagged)]
    Other(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_input_tokens: Option<u64>,
}

/// A complete assistant message, assembled from a stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub id: String,
    pub model: String,
    pub content: Vec<ContentBlock>,
    pub stop_reason: Option<StopReason>,
    pub stop_sequence: Option<String>,
    /// Set with `stop_reason: refusal`.
    pub stop_details: Option<Value>,
    pub usage: Usage,
}

impl Message {
    /// This message as the assistant turn of the next request, blocks unchanged.
    pub fn to_param(&self) -> MessageParam {
        MessageParam {
            role: Role::Assistant,
            content: self.content.clone(),
        }
    }
}
