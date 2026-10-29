# Clearing old tool results

How the REPL replaces old tool results with a placeholder to keep the prompt small, and why it has to remove thinking with them (`src/clearing.rs`, the turn start in `src/main.rs`, the `clear` record in `src/session.rs`). The API's own tool-result clearing (`context_management`, `clear_tool_uses_20250919`) is not used: the project implements clearing itself (#7, #11).

## Clearing invalidates later thinking

On `claude-haiku-5-5` a thinking block is valid only while the `system` prompt, the `tools` and every message before it are unchanged since it was produced ("preserved thinking", platform.claude.com/docs/en/build-with-claude/preserved-thinking, read raw on 2026-10-29). Clearing an earlier `tool_result` is such a change, for every later thinking block. The model thinks without being asked (a signed thinking block with empty text in `tests/fixtures/tool-loop/03.body`), so every conversation carries them. Thinking blocks may be removed from the start, from the end or all of them; removing one from the middle invalidates those after it.

So `clear_before` removes every `thinking` and `redacted_thinking` block after the first message it changed and keeps those before it. An assistant message left with no blocks gets a text block, `[Earlier reasoning cleared.]`, since a message cannot be empty. Thinking produced after a clear follows the new history and stays valid. Applying the same clear again changes nothing, so a later clear removes thinking only from where it clears a result not cleared before.

Probed on 2026-10-29 with the `tool-loop` fixture's conversation, its tool result replaced with the placeholder:

- thinking kept, beta `thinking-binding-controls-2026-08-01` with `prefix_mismatch_behavior: "error"`: 400, "Content before this block differs from when it was created, first at `messages.2.content.0`" (`req_011CgVhQ58Y6TeA9eRowwRgM`);
- the thinking after it removed: 200 (`req_011CgVhQ7PUo5zAQ4WAyghqf`);
- thinking kept, no beta and no `block_binding`: the same 400 (`req_011CgVhQJVxJus8LJYrJZkbr`), so the account this project runs on is enforced by default, as the docs say of accounts created on or after 2026-08-31.

Live through the harness on 2026-10-29: an agent turn read a 463-byte file (2 requests), `clear_before` cleared that result, and the next request was accepted (`msg_011CgVjUpUMx4XFGscJikddm`). That turn produced no thinking blocks, so the removal itself was exercised only by the hand-built probe above.

The model loses the reasoning after each clear point. In the `tool-loop` recording at effort `high`, the two replies that called tools reported `thinking_tokens: 0` and the final answer 154, so the loss may be mostly the final answers' reasoning; three replies do not show whether that holds.

Maintainer's call (2026-10-29), shown with the alternatives and a completeness pass over them: clearing in the harness, at the start of a user turn, in batches, with the thinking after it removed. The alternatives were the API dropping the invalid blocks (`prefix_mismatch_behavior: "drop_block"`: the same loss, a beta, and output tokens the docs warn can rise), the API's server-side clearing (no lost thinking, but not built here and nothing on Ollama), disabling thinking (the blocks from before still fail), and not clearing at all (bound results before the first send and leave long conversations to #8). The record that history is append-only ([agent-loop](agent-loop.md)) now holds within a turn; the REPL is what edits it, between turns.

## When it clears

Only at the start of a user turn, before the user's line is added, and not when the history ends with tool results: the docs require a tool-use turn's thinking to be sent back with its results, and clearing removes the thinking after the clear point. A turn that ends at the iteration cap, at `q`, or at a stop reason other than `tool_use` leaves results the model has not answered yet; the next turn sends them, and clearing waits a turn. Whether the API would accept that thinking removed was not probed; lens found the case. A single long turn is therefore not bounded by clearing; each result is bounded when it is first sent instead (`read_file`'s `MAX_READ_BYTES`, `shell`'s tail; [agent-loop](agent-loop.md)).

`plan` clears when the last request of the last turn kept had a prompt (uncached input plus cache reads plus cache writes, `Usage::prompt_tokens`) of `trigger_tokens` or more, keeps the `keep_results` most recent tool results, and clears only when the results it would replace hold at least `min_cleared_bytes`. The boundary is the message holding the oldest kept result, so a message is cleared whole or not at all. Results already cleared do not count, and neither do results no longer than the placeholder (78 bytes), which clearing would make longer: `shell`'s `exit code: 0` or an `edit_file` confirmation. A turn the REPL drops, cancelled or failed, does not count either: its prompt held the tool rounds that were then removed, and counting it cleared the turn after it on a fake API, which review found.

Defaults (`ClearPolicy::default`): trigger 80,000 tokens, keep 3, at least 40,000 bytes. The API's own strategy defaults to a 100,000-token trigger and keeps 3 tool uses; its minimum, `clear_at_least`, is unset by default (context-editing page, 2026-10-29). The trigger sits below 100,000 because the prompt-caching page prices Claude Haiku 5.5 prompts over 100,000 tokens five times higher ($0.10 against $0.50 per million input tokens, cache reads $0.01 against $0.05); how a prompt is counted for that line was not read. The minimum is there because every clear rewrites the cache from the clear point onward ([prompt-cache](prompt-cache.md)), so clearing a little every turn would rewrite it every turn; 40,000 bytes is a judgement, not a measurement. No trigger is known right after a resume, since usage is not kept in the session file, so the first turn of a resumed session does not clear.

## On disk

The session file keeps the original messages and adds `{"type":"clear","before":n}`; `load` applies `clear_before(history, n)` at that point, so a resumed session sends exactly what the session sent before ([sessions](sessions.md)). Keeping the decision instead of recomputing it matters because its input, the last request's usage, is not stored.

The trap: what `clear_before` produces is part of the file format. A build that changes `CLEARED`, `[Earlier reasoning cleared.]`, which results it clears or which thinking it removes would rebuild a different history from an existing file, and that session's next request would fail the thinking check. Such a change needs a new `FORMAT_VERSION`.

## The report

The REPL prints `[cleared N old tool result(s), B bytes, and T thinking block(s) after them]` and forgets the last cached request, so the next request's smaller cache read is not reported as the prefix changing.

Checked on 2026-10-29 against a local fake API: a turn reading a 60 KB file four times reported a 90,000-token prompt; the next turn cleared the first result (50,013 bytes) and 4 thinking blocks, kept the thinking before it and the 3 later results, and `--continue` afterwards sent the same messages byte for byte. A turn that failed after a 90,000-token request was followed by no clear; with the reset taken out, the same run cleared. `main.rs` has no automated test of this wiring.

## Not built

No setting or flag for the policy, no exclusion of particular tools, no clearing of `tool_use` inputs, and nothing that clears inside a turn.
