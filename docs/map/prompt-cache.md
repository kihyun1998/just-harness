# Prompt cache

How the harness shapes requests so the API's prompt cache hits, and how it shows whether it did (`Agent::run_turn` in `src/agent.rs`, `CreateMessageRequest::cache_control` and `Usage::add` in `src/api/types.rs`, the turn report in `src/main.rs`). The cache lives on the server; the harness owns only the request shape. Rules from the bundled `claude-api` skill's `shared/prompt-caching.md`, a summary of the API docs: render order is tools, then system, then messages; a byte change anywhere invalidates everything after it; at most 4 breakpoints; the minimum cacheable prefix on `claude-haiku-5-5` is listed as 512 tokens; writes cost 1.25x base input at the 5-minute TTL and reads about 0.1x.

## The two breakpoints

With `Agent::prompt_cache` on (the default), every request carries an explicit `cache_control` on the last system block, which caches tools and system together, and a top-level `cache_control` (automatic caching), which the API places on the last cacheable block and moves forward as the conversation grows. That is the combination the reference recommends for agent loops: a guaranteed read point for the static prefix that survives anything later in `messages`, plus the growing tail. It takes 2 of the 4 slots. The marker is added to a copy of the system blocks per request; the agent's own `system` stays unmarked. `add_cache_breakpoints` counts markers a caller already put on tools, system blocks or messages and adds only what still fits in 4, so the API never sees a fifth. With an empty `system` there is no block to mark and only automatic caching is sent.

`Agent::prompt_cache = false`, or `--no-cache` in the REPL, sends no `cache_control` at all, for a server that rejects it. Maintainer's call (2026-10-29, shown against leaving it to the local-model work in #10; goose has `prompt_cache_disabled`).

Maintainer's call (2026-10-29, shown with 1 hour and its 2x write): the 5-minute TTL. Requests inside a tool loop are seconds apart, so the difference is only a user who waits more than 5 minutes between lines, whose next request then writes the cache again.

## What keeps the prefix byte-stable

- The system prompt is fixed text plus the working folder, nothing time-dependent; the tool list is `builtin()` in a fixed order with descriptions built from constants.
- serde_json is built without `preserve_order`, so JSON objects serialize with sorted keys (`cargo tree -e features -i serde_json`, 2026-10-29).
- History is append-only within a turn ([agent-loop](agent-loop.md)), so each request's `messages` starts with the previous request's. Clearing old tool results between turns changes it, and the cache is written again from the first cleared result on ([tool-result-clearing](tool-result-clearing.md)).
- `effort` and thinking settings ride in `Agent::extra`, fixed for the agent; changing them mid-session invalidates the messages cache (the reference's invalidation table).

`keeps_tools_and_system_byte_identical_across_requests` compares the serialized `tools`, `system` and `model` of two requests in one tool round. The cache matches by section (tools, then system, then messages), not by the order of the JSON body's keys, which sort `messages` before `system` and `tools`; an earlier version of the test compared the bytes before `messages` and so never saw the tools, which review caught. The replay tests compare whole recorded request bodies ([messages-api-stream](messages-api-stream.md), Recorded fixtures).

## Seen live

The `tool-loop` recording of 2026-10-29: the first request wrote 1,342 tokens to the cache, the second read 1,342 and wrote 64, the third read 1,406 and wrote 248, each reading everything before it and writing only the new tail. The `max-tokens` recording, sent right after with a different effort, read 1,236 tokens, the shared tools and system, and wrote 115. `tests/replay.rs` checks the turn's summed usage against those recorded values.

## The report

`Observer::on_usage` reports each request's usage as it ends, so a turn that fails or is cancelled after some requests still shows what they cost. The REPL prints the turn's total: uncached input, tokens read from the cache, tokens written to it, and output (`Usage::add`; `TurnOutcome::usage` holds the same sum).

A miss is judged per request, not on the turn's total: in a healthy loop each request reads at least what the request before it read and wrote (the recording: 1,342 + 64, then 1,406). `Usage::cache_shortfall` is what the previous request left cached that this one did not read; the REPL reports how many requests fell short and by how many tokens. Judging the turn's total instead hid two common misses, which lens found: after the cache expired, the first request rewrites it and the second reads it, so the total is never zero; and the system marker keeps tools and system reading about 1.2k tokens, so a broken conversation prefix never makes the total zero either. No comparison is made across more than the 5-minute TTL or across a turn the REPL dropped, since both shorten the cached prefix legitimately. Checked on 2026-10-29 against a local fake API scripted with cache usage: a healthy second turn printed no flag, a third that read nothing printed `1 of 1 request(s) read 1050 fewer tokens`.

## Not built

No pre-warming, no keep-alive requests, no automatic choice of the 1-hour TTL. Whether Ollama's Messages compatibility (the later local-model goal) accepts `cache_control` has not been checked.
