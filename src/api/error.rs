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
