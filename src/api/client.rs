use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use super::error::ApiError;
use super::sse::{SseDecoder, SseEvent};
use super::stream::MessageAccumulator;
use super::transport::{HttpRequest, HttpResponse, ReqwestTransport, Transport};
use super::types::{CreateMessageRequest, Message};

pub const DEFAULT_BASE_URL: &str = "https://api.anthropic.com";
pub const API_VERSION: &str = "2026-10-21";

#[derive(Clone)]
pub struct ClientConfig {
    pub api_key: String,
    pub base_url: String,
    /// Retries after the first attempt, for retryable failures before the stream starts.
    pub max_retries: u32,
    /// The first retry's delay; each later one doubles, up to `max_retry_delay`.
    pub initial_retry_delay: Duration,
    pub max_retry_delay: Duration,
}

impl ClientConfig {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            base_url: DEFAULT_BASE_URL.to_string(),
            max_retries: 2,
            initial_retry_delay: Duration::from_millis(500),
            max_retry_delay: Duration::from_secs(8),
        }
    }

    /// Reads `ANTHROPIC_API_KEY`, and `ANTHROPIC_BASE_URL` when set.
    pub fn from_env() -> Result<Self, ApiError> {
        let api_key = std::env::var("ANTHROPIC_API_KEY")
            .ok()
            .filter(|k| !k.is_empty())
            .ok_or_else(|| ApiError::Config("ANTHROPIC_API_KEY is not set".into()))?;
        let mut config = Self::new(api_key);
        if let Some(url) = std::env::var("ANTHROPIC_BASE_URL")
            .ok()
            .filter(|u| !u.is_empty())
        {
            config.base_url = url;
        }
        Ok(config)
    }
}

/// Prints `api_key` as `[redacted]`.
impl std::fmt::Debug for ClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientConfig")
            .field("api_key", &"[redacted]")
            .field("base_url", &self.base_url)
            .field("max_retries", &self.max_retries)
            .field("initial_retry_delay", &self.initial_retry_delay)
            .field("max_retry_delay", &self.max_retry_delay)
            .finish()
    }
}

pub struct Client<T: Transport = ReqwestTransport> {
    config: ClientConfig,
    transport: T,
}

impl Client<ReqwestTransport> {
    pub fn new(config: ClientConfig) -> Result<Self, ApiError> {
        Ok(Self::with_transport(config, ReqwestTransport::new()?))
    }
}

impl<T: Transport> Client<T> {
    pub fn with_transport(config: ClientConfig, transport: T) -> Self {
        Self { config, transport }
    }

    /// Sends `request` as a stream and returns the assembled message. Every
    /// stream event's JSON is handed to `on_event` as it arrives. Retries
    /// happen only before the stream starts; a failure after that is returned.
    pub async fn send(
        &self,
        request: &CreateMessageRequest,
        mut on_event: impl FnMut(&Value),
    ) -> Result<Message, ApiError> {
        let http = self.http_request(request)?;
        let response = self.send_with_retries(http).await?;
        let mut body = response.body;
        let mut decoder = SseDecoder::new();
        let mut accumulator = MessageAccumulator::new();
        while let Some(chunk) = body.next().await {
            let chunk = chunk?;
            for event in decoder.push(&chunk) {
                handle(&mut accumulator, event, &mut on_event)?;
            }
        }
        for event in decoder.finish() {
            handle(&mut accumulator, event, &mut on_event)?;
        }
        accumulator.finish()
    }

    fn http_request(&self, request: &CreateMessageRequest) -> Result<HttpRequest, ApiError> {
        let mut body =
            serde_json::to_value(request).map_err(|e| ApiError::Protocol(e.to_string()))?;
        body["stream"] = Value::Bool(true);
        Ok(HttpRequest {
            url: format!("{}/v1/messages", self.config.base_url.trim_end_matches('/')),
            headers: vec![
                ("content-type".into(), "application/json".into()),
                ("anthropic-version".into(), API_VERSION.into()),
                (
                    "authorization".into(),
                    format!("Bearer {}", self.config.api_key),
                ),
            ],
            body: serde_json::to_vec(&body).map_err(|e| ApiError::Protocol(e.to_string()))?,
        })
    }

    async fn send_with_retries(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        let mut attempt = 0;
        loop {
            let (error, retry, retry_after) = match self.transport.send(request.clone()).await {
                Ok(response) if (200..300).contains(&response.status) => return Ok(response),
                Ok(response) => {
                    let should_retry = match response.header("x-should-retry") {
                        Some("true") => Some(true),
                        Some("false") => Some(false),
                        _ => None,
                    };
                    let retry_after = retry_after(&response);
                    let error = status_error(response).await;
                    let retry = should_retry.unwrap_or_else(|| error.is_retryable());
                    (error, retry, retry_after)
                }
                Err(error) => {
                    let retry = error.is_retryable();
                    (error, retry, None)
                }
            };
            if !retry || attempt >= self.config.max_retries {
                return Err(error);
            }
            let backoff = self
                .config
                .initial_retry_delay
                .saturating_mul(1 << attempt.min(16))
                .min(self.config.max_retry_delay);
            tokio::time::sleep(retry_after.unwrap_or(backoff)).await;
            attempt += 1;
        }
    }
}

/// The server's requested wait: `retry-after-ms`, else `retry-after` in
/// seconds. Zero, negative and unparsable values give `None`.
fn retry_after(response: &HttpResponse) -> Option<Duration> {
    let parse = |name: &str| response.header(name)?.trim().parse::<f64>().ok();
    let seconds = parse("retry-after-ms")
        .map(|ms| ms / 1000.0)
        .or_else(|| parse("retry-after"))?;
    (seconds.is_finite() && seconds > 0.0)
        .then(|| Duration::from_secs_f64(seconds.min(MAX_RETRY_AFTER_SECS)))
}

/// The ceiling on a server-requested wait.
const MAX_RETRY_AFTER_SECS: f64 = 4_294_967.0;

async fn status_error(response: HttpResponse) -> ApiError {
    let status = response.status;
    let mut bytes = Vec::new();
    let mut body = response.body;
    while let Some(Ok(chunk)) = body.next().await {
        bytes.extend_from_slice(&chunk);
    }
    let body = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
    ApiError::Status { status, body }
}

fn handle(
    accumulator: &mut MessageAccumulator,
    event: SseEvent,
    on_event: &mut impl FnMut(&Value),
) -> Result<(), ApiError> {
    match event.event.as_deref() {
        None | Some("ping") => return Ok(()),
        Some("error") => {
            let body = serde_json::from_str(&event.data).unwrap_or(Value::String(event.data));
            return Err(ApiError::Stream { body });
        }
        Some(_) => {}
    }
    if event.data.is_empty() {
        return Ok(());
    }
    let data: Value = serde_json::from_str(&event.data)
        .map_err(|e| ApiError::Protocol(format!("event data is not JSON: {e}")))?;
    on_event(&data);
    accumulator.apply(&data)
}
