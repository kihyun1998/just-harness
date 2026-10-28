//! The HTTP seam: the client builds a request, a transport sends it.

use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};

use super::error::ApiError;

pub type BodyStream = Pin<Box<dyn Stream<Item = Result<Bytes, ApiError>> + Send>>;

#[derive(Clone, PartialEq)]
pub struct HttpRequest {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// Prints `authorization` and `x-api-key` values as `[redacted]`.
impl std::fmt::Debug for HttpRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let headers: Vec<(&str, &str)> = self
            .headers
            .iter()
            .map(|(k, v)| {
                let secret =
                    k.eq_ignore_ascii_case("authorization") || k.eq_ignore_ascii_case("x-api-key");
                (k.as_str(), if secret { "[redacted]" } else { v.as_str() })
            })
            .collect();
        f.debug_struct("HttpRequest")
            .field("url", &self.url)
            .field("headers", &headers)
            .field("body", &String::from_utf8_lossy(&self.body))
            .finish()
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: BodyStream,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

pub trait Transport: Send + Sync {
    /// Sends one request. `Err` means no response arrived at all.
    fn send(
        &self,
        request: HttpRequest,
    ) -> impl Future<Output = Result<HttpResponse, ApiError>> + Send;
}

pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest wait for the next bytes of a response, reset by every read.
pub const READ_TIMEOUT: Duration = Duration::from_secs(600);

/// The real transport, over `reqwest`.
#[derive(Debug, Clone)]
pub struct ReqwestTransport {
    client: reqwest::Client,
}

impl ReqwestTransport {
    pub fn new() -> Result<Self, ApiError> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| ApiError::Config(error_chain(&e)))?;
        Ok(Self { client })
    }
}

impl Transport for ReqwestTransport {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        let mut builder = self.client.post(&request.url).body(request.body);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = builder.send().await.map_err(|e| {
            if e.is_builder() {
                ApiError::Config(error_chain(&e))
            } else {
                ApiError::Transport(error_chain(&e))
            }
        })?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(k, v)| Some((k.as_str().to_string(), v.to_str().ok()?.to_string())))
            .collect();
        let body = response
            .bytes_stream()
            .map(|chunk| chunk.map_err(|e| ApiError::Transport(error_chain(&e))));
        Ok(HttpResponse {
            status,
            headers,
            body: Box::pin(body),
        })
    }
}

/// The error and each of its sources, joined with `: `.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}
