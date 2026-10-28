//! The one path from the harness to a model: the Anthropic Messages API.

mod client;
mod error;
mod sse;
mod stream;
mod transport;
mod types;

pub use client::{API_VERSION, Client, ClientConfig, DEFAULT_BASE_URL};
pub use error::ApiError;
pub use transport::{
    BodyStream, CONNECT_TIMEOUT, HttpRequest, HttpResponse, READ_TIMEOUT, ReqwestTransport,
    Transport,
};
pub use types::{
    BlockKind, ContentBlock, CreateMessageRequest, Message, MessageParam, Role, StopReason, Tool,
    Usage,
};
