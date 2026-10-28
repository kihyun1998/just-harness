# Talking to the Messages API

How `src/api/` sends a request and turns the streamed reply into one `Message`, and why it is shaped that way. Wire facts come from the official Python SDK (anthropic-sdk-python v2.3.0: `src/anthropic/_streaming.py`, `src/anthropic/lib/streaming/_messages.py`, `src/anthropic/_base_client.py`, `src/anthropic/_constants.py`, the generated types), because the published docs are a summarized source.

## Content blocks are kept as raw JSON

`ContentBlock` wraps a `serde_json::Value` instead of typed fields. A thinking block must go back to the API exactly as received, `signature` included, when tools are in use (the SDK's `ThinkingBlock.signature` docstring says so). Typed fields would drop any field or block type this crate does not know yet, and the next request would send an altered history. `BlockKind` is the typed view for code that needs to read a block.

## Stream assembly (`MessageAccumulator`)

- Usage on `message_delta` is a running total. It replaces the value from `message_start`; adding the two double-counts.
- Tool input arrives as JSON text split across `input_json_delta` events. It is buffered per block index and parsed once, in `finish`, after `message_delta` has set the stop reason.
- Tool input that does not parse fails the message, except when the stop reason is `max_tokens`: then the block keeps the input it started with (usually `{}`) and the message, with the text and usage already paid for, is still returned. The agent loop does not run tool calls from such a reply and answers them as not run ([agent-loop](agent-loop.md)). Whether the API sends `content_block_stop` for a block cut by `max_tokens` has not been observed.
- A stream that ends without `message_stop` is a `Transport` error: the connection broke off. An event before `message_start` is a `Protocol` error.
- SSE events with no `event:` name are skipped, as the SDK does. `ping` is skipped. An `error` event becomes `ApiError::Stream`.

## SSE decoding (`SseDecoder`)

- A `\r` that is the last byte held is not yet a line end: the next chunk may start with `\n`, and `\r\n` is one line end. `finish` settles it when the stream ends.
- An event with no closing blank line at the end of the stream is dropped, as in the SDK and the SSE spec.
- A comment line (`: keep-alive`) needs no branch of its own: it splits into an empty field name, and unknown field names are ignored.

## Retries and timeouts (`Client::send_with_retries`, `ReqwestTransport`)

- Retries happen only before the stream starts. A failure after a 200 is returned to the caller, as in the SDK. That includes an in-stream `error` event such as `overloaded_error`: goose retries it, the SDK does not, and the maintainer chose the SDK's behaviour on 2026-10-28 (shown both, with the cost of discarding a partial reply), leaving any resend to the agent loop. This is a judgement, theirs to reverse.
- Retryable: 408, 409, 429, 5xx, and transport failures. A server `x-should-retry: true|false` overrides the status code.
- The wait is `retry-after-ms`, else `retry-after` in seconds, honoured at any positive value up to `MAX_RETRY_AFTER_SECS`, the SDK's own ceiling. Zero or unparsable falls back to the doubling backoff in `ClientConfig::new`, whose defaults are the SDK's `INITIAL_RETRY_DELAY`, `MAX_RETRY_DELAY` and `DEFAULT_MAX_RETRIES`. No jitter yet, and the HTTP-date form of `retry-after` is not read.
- `reqwest` has no timeout by default. `CONNECT_TIMEOUT` and `READ_TIMEOUT` (a read timeout, reset by every read) take the SDK's `DEFAULT_TIMEOUT` values and catch a stalled stream.

## Auth

The key goes in `Authorization: Bearer`, as the SDK sends it. Live calls on 2026-10-28 (`examples/smoke.rs`, against `claude-haiku-5-5`) were accepted with it and `anthropic-version: 2026-10-21`. `Debug` on `ClientConfig` and `HttpRequest` prints the key as `[redacted]`.

## Seen live

From the `examples/smoke.rs` runs on 2026-10-28: a short text reply was 6 events, a tool call 10. At `output_config.effort: "high"` the reply to a tool result carried a thinking block with empty `thinking` text and a 932-character `signature`. No tool-call reply has carried a thinking block yet, so sending one back to the API has not been observed; the fake-transport test covers it.

## Measured

Appending text deltas by rebuilding the string, and draining the SSE buffer once per line, made assembly quadratic: 40,000 deltas (400 KB of text) took 13.5 s in a release build. Appending in place and draining once per chunk brought the same input to 33 ms (5,000 deltas: 198 ms to 8 ms). Measured with a throwaway example on 2026-10-28.
