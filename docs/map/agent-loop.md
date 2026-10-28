# The agent loop and its tools

How `src/agent.rs` runs a turn and how the tools in `src/tools/` behave, and why. The loop rules come from the Python SDK's tool runner (anthropic-sdk-python v2.3.0, `src/anthropic/lib/tools/_beta_runner.py`), read from source; the tool limits from goose (`crates/goose/src/agents/platform_extensions/developer/{shell,edit}.rs`).

## What a stop reason means (`next_step`)

- `tool_use`: run the turn's tool calls and send the results.
- `pause_turn`: send the conversation back unchanged and run nothing; the server continues the turn. The SDK maps `compaction` the same way; this crate has no `compaction` stop reason because server-side compaction is not used.
- Everything else, including an absent or unknown value, stops the turn and its `tool_use` blocks are **not** run. That is what keeps a tool call cut by `max_tokens` (whose input is left as `{}`, see [messages-api-stream](messages-api-stream.md)) from running.

## A reply that stops with tool calls still in it

A reply cut at `max_tokens`, or ending with `refusal` or any stop reason other than `tool_use`, can still hold `tool_use` blocks. They are not run, and `run_turn` answers each with an `is_error` result, `Not run: the reply stopped at <reason>.`, in one user message. Without it the next request fails: a probe on 2026-10-28 got `400 invalid_request_error`, "`tool_use` ids were found without `tool_result` blocks immediately after" (request `req_011CgUGTzrX8grgjzSqofRQz`). The same probe showed two user messages in a row are accepted, so the user's next line can follow those results directly. Maintainer's call, shown against dropping the cut reply or leaving it.

## Tool results

- One user message carries every result of a turn, in the order of the `tool_use` blocks, right after the assistant message. The SDK runner does the same. Claude Code's bundled `claude-api` skill adds that splitting them across messages trains the model to stop calling tools in parallel; that claim was not checked against the API docs.
- An unknown tool name answers `Error: Tool '<name>' not found` with `is_error`, as the SDK does; a failing tool answers its error text with `is_error`. Neither stops the loop.
- Calls in one turn run one after another, not concurrently, so two edits or commands from one reply cannot interleave.

## History is append-only

`run_turn` only pushes: the assistant message through `Message::to_param` (blocks unchanged, thinking `signature` included) and the tool-result message. Each request's `messages` is the previous request's plus two entries; `tests/agent.rs` checks the prefix byte for byte. Prompt caching (#6) depends on this.

## Maintainer's calls (2026-10-28, each shown with its alternative)

- **Iteration cap:** 50 requests per turn by default, `--max-iterations N` to change it. The SDK has no default cap. Reaching it ends the turn as `TurnEnd::IterationLimit`.
- **Folder confinement:** `read_file`, `edit_file` and `search` refuse any path outside the working folder: absolute, `../`, or a symlink leading out. `shell` is not confined, so this guards against mistakes, not a hostile model. goose allows any absolute path.
- **Secrets:** `read_file` and `edit_file` refuse `.env` and `.env.*` (not `.env.example`), also when reached through a symlink, so the API key in the working folder's `.env` does not land in the conversation or on screen. `search` already skips hidden files; `shell` can still read it. Chosen 2026-10-28, shown against leaving it readable.
- **Entry point:** a terminal REPL in `src/main.rs`, not a library plus example.

These are judgements, the maintainer's to reverse.

## Path resolution (`ToolContext::resolve`)

`.` and `..` are folded first; then the walk goes up to the longest part of the path that exists **as a directory entry** (`symlink_metadata`, which does not follow links), canonicalizes it, and appends the rest; the result must start with the canonical working folder. Testing existence with `exists()` follows links, so a dangling symlink inside the folder read as "not there yet" and `edit_file` could create the file it points to outside the folder; review found it, `a_dangling_symlink_cannot_lead_a_new_file_outside` pins it. Folding `..` before resolving symlinks is safe because the tools open the resolved path, never the one they were given.

## Tool failures

`ToolSet::call` turns a panicking tool into an `is_error` result (`Error: tool '<name>' panicked: ...`) so the loop goes on, as the issue asks and as the SDK runner does for any exception.

## read_file

Numbered lines, `DEFAULT_READ_LIMIT` lines by default, at most `MAX_READ_BYTES` per call (the rest is left for the next `offset`), lines past `MAX_LINE_CHARS` characters cut with a note. `str::lines` drops a trailing `\r`, so a CRLF file reads like an LF one.

## edit_file

Exactly one match of `old_string`, else an error naming the match count and the first lines. When `old_string` has `\n` and only matches with `\r\n` (a CRLF file, text copied from `read_file`), both strings are turned to CRLF and the file keeps its line endings; codex preserves line endings the same way, goose does not. An empty `old_string` writes a file that does not exist or is empty, and refuses one with content, so nothing is overwritten. Not in the issue; the maintainer kept it on 2026-10-28 and asked for the empty-file case (shown against keeping it create-only or removing it).

## Shell

- `sh -c` in the working folder, stdin closed, each command in its own process group. At the timeout the whole group gets SIGKILL: `kill_on_drop` alone kills only `sh`, and a pipeline like `sleep 30 | cat` would outlive it. The test checks with `pgrep` that nothing of the command survives.
- The command is done when the process exits; output is read for `OUTPUT_DRAIN` more, then dropped. Waiting for the pipes to close instead made `sleep 30 & echo started` block until the background job ended (found by lens, measured 3.1 s for `sleep 3 &`). goose drains 500 ms, codex 2 s. A background job keeps running after the command returns.
- A timeout is an error, and still returns what the command printed before it.
- A non-zero exit code is reported in the result, not marked `is_error`.
- Each of stdout and stderr keeps its **last** `OUTPUT_LIMIT_LINES` lines or `OUTPUT_LIMIT_BYTES` bytes, goose's limits and goose's side, since a failing test prints its summary at the end. Only that tail is held in memory while the command runs. goose also saves the full output to a file; this does not.
- Unix only (`process_group`, `kill`).

## Search

Walks with the `ignore` crate: hidden files (so `.env`, `.github/`) and `.gitignore` paths are skipped, and `require_git(false)` applies `.gitignore` even outside a git repository; "no matches" says so, so a skipped folder is not read as empty. A glob without `/` matches file names anywhere, like ripgrep's `-g`; a glob with `/` matches the path from the working folder. Results stop at `MAX_RESULTS` with a note.

## The REPL

- **Ctrl-C during a request** drops the running turn: its future is dropped, `KillGroupOnDrop` in the shell tool kills the command's process group, and the turn's messages leave the history. Edits already made stay. **Ctrl-C or Ctrl-D at the prompt** quits. Without a handler, SIGINT killed only the harness and left the command running, since its own process group keeps it out of the terminal's. Maintainer's call, shown against quitting-and-killing and leaving it to #10. Checked on 2026-10-28 against a local fake API: the command's processes went from 2 to 0, the REPL kept going, a second SIGINT at the prompt quit.
- Input is read on a thread of its own, not with `tokio::io::stdin`, whose blocking read cannot be cancelled and kept the process alive after Ctrl-C at the prompt until Enter (tokio's docs say so; the fake-API check showed it).

Reads `ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL` through `ClientConfig::from_lookup`, from the environment, else from `./.env`; it does not set environment variables, since `std::env::set_var` is `unsafe` in Rust 2024. Model text and tool output are printed with control characters other than newline and tab replaced by `�`, so a file cannot send escape sequences to the terminal; that replacement has no test. Each input line is one request. When a turn fails with an API error, the turn's messages are dropped from the history so the conversation stays well-formed; side effects of tools that already ran are not undone.
