use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum ApiError {
    /// The API answered with a non-success status before streaming.
    Status { status: u16, body: Value },
    /// An `error` event arrived in the middle of a stream.
    Stream { body: Value },
    /// The request never got an answer, or the response broke off:
    /// connection, timeout, a body cut short.
    Transport(String),
    /// The stream broke the event protocol, or a payload did not parse.
    Protocol(String),
    /// Missing or unusable configuration.
    Config(String),
}

impl ApiError {
    /// Whether sending the same request again may succeed: 408, 409, 429,
    /// any 5xx, and transport failures.
    pub fn is_retryable(&self) -> bool {
        match self {
            ApiError::Status { status, .. } => matches!(status, 408 | 409 | 429) || *status >= 500,
            ApiError::Transport(_) => true,
            ApiError::Stream { .. } | ApiError::Protocol(_) | ApiError::Config(_) => false,
        }
    }

    /// Whether the API refused the request for its size: a 413, or a 400
    /// saying the prompt is too long.
    pub fn is_prompt_too_long(&self) -> bool {
        let ApiError::Status { status, body } = self else {
            return false;
        };
        match status {
            413 => true,
            400 => body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.to_lowercase().contains("prompt is too long")),
            _ => false,
        }
    }

    /// What a 400 says about a content block the API could not process.
    pub fn refused_block(&self) -> Option<RefusedBlock> {
        let ApiError::Status { status: 400, body } = self else {
            return None;
        };
        if self.is_prompt_too_long() {
            return None;
        }
        let message = body["error"]["message"].as_str()?;
        let lower = message.to_lowercase();
        let kind = if lower.contains("image") {
            "image"
        } else if lower.contains("pdf") {
            "document"
        } else {
            return None;
        };
        let (place, reason) = match message.split_once(": ") {
            Some((path, reason)) if path.starts_with("messages.") => (block_place(path), reason),
            _ => (None, message),
        };
        Some(RefusedBlock {
            kind,
            reason: reason.to_string(),
            place,
        })
    }
}

/// A content block the API could not process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefusedBlock {
    /// `image` or `document`.
    pub kind: &'static str,
    /// The reason the API gives, without the block's path.
    pub reason: String,
    /// The message, the block in it and the block inside a tool result, when
    /// the API names them.
    pub place: Option<(usize, usize, Option<usize>)>,
}

/// The place `messages.<m>.content.<b>[.tool_result.content.<n>]...` names.
fn block_place(path: &str) -> Option<(usize, usize, Option<usize>)> {
    let parts: Vec<&str> = path.split('.').collect();
    let number = |i: usize| parts.get(i)?.parse::<usize>().ok();
    let nested = match (parts.get(4), parts.get(5)) {
        (Some(&"tool_result"), Some(&"content")) => Some(number(6)?),
        _ => None,
    };
    Some((number(1)?, number(3)?, nested))
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Status { status, body } => write!(f, "API returned {status}: {body}"),
            ApiError::Stream { body } => write!(f, "error event in stream: {body}"),
            ApiError::Transport(msg) => write!(f, "transport error: {msg}"),
            ApiError::Protocol(msg) => write!(f, "protocol error: {msg}"),
            ApiError::Config(msg) => write!(f, "configuration error: {msg}"),
        }
    }
}

impl std::error::Error for ApiError {}
