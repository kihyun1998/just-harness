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

## Approval

- A tool says whether its calls need approval (`Tool::needs_approval`, a required method with no default, so a new tool cannot forget to answer; review found the earlier `false` default failed open). `shell` and `edit_file` do, `read_file` and `search` do not. `run_turn` takes an `Approver` and asks it right before running such a call, in call order. There is no default approver either: callers pass `AllowAll` to skip asking, so leaving approval out is a visible choice.
- The tool also supplies what the user is shown (`approval_preview`) and what an allow-for-the-session covers (`approval_key`); by default both are the whole input, so a tool that says nothing more is shown everything and allowed only for that exact input. `shell` shows the command and any timeout, keyed by the command string. `edit_file` shows the path, the file it resolves to when that differs (a symlink), then the old text as `-` lines and the new as `+` lines, keyed by the path as given.
- `Approval::Deny(reason)` sends `The user denied this call.` plus the reason as an `is_error` result, so the model can take another route. `Approval::Stop` answers that call and every later call of the reply `Not run: the user stopped the turn.`, sends them, takes one more reply even when that passes the iteration cap, answers any tool calls in it the same way, and ends with `TurnEnd::StoppedByUser`. Unlike Ctrl-C, the model's answer stays in the history. That extra reply is a paid request; if it fails, the REPL drops the turn as for any API error, edits already made stay.
- Prior art: codex's `ReviewDecision` (`approved`, `approved_for_session`, `denied`, `abort`, `timed_out`, in `codex-rs/app-server-protocol`). Its `approved_for_session` approves "future prompts in the same session-scoped approval cache"; how codex keys that cache was not read.
- Maintainer's call (2026-10-28, shown against `y/n/a` and `y/n`): the REPL offers `y`, `n [reason]`, `a` and `q`. Text after `y`, `a` or `q` is refused and asked again, so a condition typed after a yes is not silently dropped. A preview over 40 lines is cut with a count. Control characters and invisible format characters (bidi controls, zero-width characters) are shown as `�`, so what the prompt shows is what runs.
- Lines already queued when a call comes up for approval are dropped, and the REPL says how many, also when the call is allowed for the session, so a pasted block cannot answer a question it was typed before or reach the model as the next request. Checked on 2026-10-28 against a local fake API, with each answer; the `Prompt` approver itself has no automated test.

## History is append-only

`run_turn` only pushes: the assistant message through `Message::to_param` (blocks unchanged, thinking `signature` included) and the tool-result message. Each request's `messages` starts with the previous request's, unchanged: two entries longer after a round of tool calls, one after a `pause_turn` resend. `tests/agent.rs` checks that prefix as parsed JSON. The prompt cache depends on this ([prompt-cache](prompt-cache.md)). Between turns the REPL may clear old tool results, which edits earlier messages ([tool-result-clearing](tool-result-clearing.md)); `run_turn` itself never does. `Agent::tool_definitions`, when set, is sent in place of the tool set's definitions, so a resumed session carries the definitions it recorded while calls run through this build's tools ([sessions](sessions.md)). `Agent::send` sends one request with the same model, system, tools, extra fields and cache markers as a turn's requests and runs none of the reply's tool calls; `run_turn` sends through it, and the compaction's summary request uses it ([compaction](compaction.md)).

## Maintainer's calls (2026-10-28, each shown with its alternative)

- **Iteration cap:** 50 requests per turn by default, `--max-iterations N` to change it. The SDK has no default cap. Reaching it ends the turn as `TurnEnd::IterationLimit`.
- **Folder confinement:** `read_file`, `edit_file` and `search` refuse any path that leads outside the working folder, whether absolute, through `../` or through a symlink; an absolute path inside it is allowed, and so is one inside a folder allowed beside it with `ToolContext::allow`, which the REPL uses for the memory folder ([memory](memory.md)). `shell` is not confined, so this guards against mistakes, not a hostile model. goose allows any absolute path.
- **Secrets:** `read_file` and `edit_file` refuse `.env` and `.env.*` (not `.env.example`), also when reached through a symlink, so the API key in the working folder's `.env` does not land in the conversation or on screen. `search` already skips hidden files; `shell` can still read it. Chosen 2026-10-28, shown against leaving it readable.
- **Entry point:** a terminal REPL in `src/main.rs`, not a library plus example.

These are judgements, the maintainer's to reverse.

## Path resolution (`ToolContext::resolve`)

`.` and `..` are folded first; then the walk goes up to the longest part of the path that exists **as a directory entry** (`symlink_metadata`, which does not follow links), canonicalizes it, and appends the rest; the result must start with the canonical working folder. Testing existence with `exists()` follows links, so a dangling symlink inside the folder read as "not there yet" and `edit_file` could create the file it points to outside the folder; review found it, `a_dangling_symlink_cannot_lead_a_new_file_outside` pins it. Folding `..` before resolving symlinks is safe because the tools open the resolved path, never the one they were given.

## Tool failures

`ToolSet::call` turns a panicking tool into an `is_error` result (`Error: tool '<name>' panicked: ...`) so the loop goes on, as the issue asks and as the SDK runner does for any exception.

## read_file

Numbered lines, `DEFAULT_READ_LIMIT` lines by default, at most `MAX_READ_BYTES` per call (the rest is left for the next `offset`), lines past `MAX_LINE_CHARS` characters cut with a note. `str::lines` drops a trailing `\r`, so a CRLF file reads like an LF one.

### What a file is read as

`file_kind` decides by the extension, case aside: `.png`, `.jpg`, `.jpeg`, `.gif`, `.webp` are images, and an extension in `BINARY_EXTENSIONS` is refused. A file with no extension is an image when its first bytes say so, and text otherwise. Any other extension is text, so a PNG named `.txt` gets the `not UTF-8 text` error. Measured on Claude Code 2.1.319 in `-p` mode against a local fake API (2026-11-07), except that Claude Code shows such a file as garbled text with a notice about NUL bytes, where this keeps its error. `.zip`, `.bin`, `.heic`, `.dat` were measured refused and `.xlsx`, `.parquet` are in the tools-reference page; the rest of `BINARY_EXTENSIONS` is a judgement, not a measurement.

### Images

An image file's format comes from its bytes, not its extension, so a PNG named `.jpg` goes as `image/png`; a wrong `media_type` is what the name would have sent. Claude Code does the same on the same measurement. An image extension over something else is an error that names the likely cause, as is an empty file; both follow Claude Code's. A file that starts right and does not decode is an error too, as in codex (`view_image.rs`), so the model hears about it instead of the request carrying it; what the API does with such an image was not measured.

- **Sizes (measured on Claude Code):** a long edge over 2000 px is scaled to 2000 (4000x3000 to 2000x1500, 2001x100 to 2000x100), and a text block after the results says the original size and the factor, in Claude Code's words. A long edge of exactly 2000 is unchanged, with no text. Claude Code gave 9000x100 as 1980x22 while saying 2000x22, so its rounding is not followed exactly. The factor is the ratio of the long edges, which a short edge rounded to a few pixels would distort.
- **Why resize at all:** the API takes up to 8000x8000 px and 10 MB of base64 per image and downsizes by itself (vision page, read raw 2026-11-07), so this is not needed for a single image. Past 20 images in a request a stricter limit applies to all of them, 3000 px being safe by that page, and images from earlier turns count, so a session of large unresized images could start failing there. The session file also stays small. Maintainer's call (2026-11-07), shown against refusing only what is over the API's limits, with no new dependency: 111 to 134 unique crates, `image` and `base64` counted; build time and binary size not measured.
- **The JPEG rule:** an image over `MAX_BYTES` (1,000,000) is sent as JPEG, quality 80 down to 20 until it fits, the last whatever its size. A source that is not a JPEG tries PNG first, resized or not, so a PNG can stay a PNG by being re-encoded. Claude Code kept a noise PNG of 270 KB and converted one of 1.08 MB, and sent about 500 KB for 3 MB and 12 MB ones, so the limit lies between those two sizes; 1,000,000 and the qualities are a judgement, not a measurement. Claude Code also scaled the 12 MB one, 2000x2000, to 1932x1932; this keeps 2000.
- **Transparency** is blended onto white when a JPEG is made: JPEG has no alpha, and dropping it leaves whatever colour transparent pixels hold, usually black, under dark text.
- A file over 128 MB is refused without being read, in Claude Code's words (errors page). A path that is not a regular file is refused too. An image over the `image` crate's default decoding limit, 512 MiB, is an error saying to make a smaller copy: 16000x12000 RGB needs 576 MB. That limit bounds memory and not time: a review pass measured 0.58 s and 838 MB for a 2 MB PNG of 12000x12000 on a release build.

### Notebooks

`.ipynb` comes back as text and image blocks in the order they fall, in the format Claude Code 2.1.319 sent against a local fake API (2026-11-07): each cell as `<cell id="x">source</cell id="x">`, with `<cell_type>` for anything but code, `<language>` for code when `metadata.language_info.name` is not python, and `cell-<index>` when a cell has no id. Each output follows as a line end and its text: a stream's text, `text/plain`, or `name: value` and the traceback. `text/html` is dropped. These are the ids #24 will name cells by.

- **Limits:** content past 256 KB is an error with `jq` commands, and a file past 100 MB or one that is not a regular file is refused unread (tools-reference page). What counts toward the 256 KB here is the cells' text without the line ends between them, and image data; how Claude Code counts was measured only on one 300 KB cell, reported as 293KB. Text past 25,000 tokens is an error too (same page, "or the token limit"). Claude Code counted 30,452 tokens over 25,000 on a 132,800-byte notebook, reported by a review pass, and it asks `count_tokens`, which this project left out; here tokens are estimated at 4 bytes each, so a notebook of many short tokens can differ from it.
- **Large outputs:** one cell's outputs over `MAX_OUTPUT_BYTES` (10,000 bytes of text and image data together) are replaced with a line naming the `jq` command. Claude Code kept an image of 9,760 base64 bytes and replaced one of 12,264, and replaced 20,000 characters of text and 1,000 tiny images; 10,000 lies in that range, its exact value unmeasured. Its text says `Bash`; here it says a shell command, since the tool is `shell`.
- **Images:** take the same path as an image file, without the resize note, which Claude Code's small image never showed. The format comes from the bytes, where Claude Code sent a PNG under `image/jpeg` as the notebook labelled it. An output that does not decode becomes a line of text saying so; Claude Code's handling was not measured. Line ends inside the base64 are allowed.
- A notebook's cell count has no limit of its own, so about 2,600 cells of tiny images fit under 256 KB and would pass the API's 600 images. Not measured on Claude Code.
- A notebook's other errors follow Claude Code's wording: not JSON, and no `cells` array or a cell with no string or string-array `source`.

### PDFs

Measured on Claude Code 2.1.319 against a local fake API (2026-11-07) and followed, wording included, except where noted:

- **Whole:** up to 10 pages, a text line and a `document` block. Over that, an error asking for `pages`. A page range is one page or one range of at most 20; anything else is an error, and a range running past the end is cut at the last page. A range starting past the end is an error. `pages` on anything but a PDF is ignored.
- **Pages:** each page goes as a JPEG image, rendered by `pdftoppm` at 100 DPI: a 200-point page came back as 278 px. Through the same size rules as an image file. Started with `kill_on_drop` from `tokio::process`, so a Ctrl-C ends a slow render.
- **Without poppler (measured with Homebrew off the `PATH`):** the page count is unknown, so a 12-page PDF is sent whole, and a range is the `pdftoppm is not installed` error. So 11 pages or more may reach the API here; its limit is 600 pages, 100 when the context window is under 1M tokens (pdf-support page; Claude Haiku 5.5's is 1M, [compaction](compaction.md)), and what comes of going over it was not measured. `pdfinfo` as the source of the count is this project's choice, since removing Homebrew removes both of poppler's tools; which one Claude Code uses was not read.
- **`MAX_WHOLE_BYTES` (90,000,000) is not Claude Code's:** the pdf-support page limits a request to 128 MB, and base64 adds a third, so a whole PDF over this could not be sent. The limit is per request, so two smaller ones can pass it together. Claude Code's errors page speaks of "the API's 32MB limit" and "max 100 pages, 20MB", which disagrees; which holds was not checked, since it takes a live key. Claude Code's handling of a large PDF was not measured.
- **`pdfinfo`'s output is read from the end:** a title can hold a line saying `Pages: 1`, and the title comes first, which a review pass reproduced. `pdftoppm`'s stderr is discarded, because a small broken PDF made it print over 400 KB, which went into the error and the conversation (review pass: 29,969 bytes gave 423,071); Claude Code says only that the file is corrupted or invalid.
- A 10-page PDF is not small: the pdf-support page gives 1,500-3,000 tokens of text per page plus each page as an image, up to 4,784 visual tokens at this tier, so as much as about 78,000 tokens by those figures, close to the triggers. Clearing and compaction run only between turns ([tool-result-clearing](tool-result-clearing.md)). Not measured; this needs a live key. A document's base64 counts by its bytes for clearing.

A `ToolOutput` with `blocks` sends them as the result's `content`, and `content` becomes only what the terminal shows, so base64 cannot reach the screen (checked against the fake API on 2026-11-07). A text block goes after all of a reply's results because the API requires text after `tool_result` blocks in a user message (handle-tool-calls page). Results with no blocks stay strings, byte for byte.

Not for Ollama yet: its compatibility page lists `document` blocks as unsupported and base64 images as supported, and does not say whether an image nested in a `tool_result` works. Read on 2026-11-07, not run. `read_file` here has no setting for it; #10 holds the local-model goal.

Not checked: whether the API accepts these blocks from this harness. The key in `.env` still got a 401 on 2026-11-07 (`GET /v1/models`), so the shape rests on the docs and on it being what Claude Code sends. EXIF orientation is not applied, so a rotated photo that gets re-encoded loses its rotation; Claude Code's handling was not measured. Decoding runs on the async task without yielding, so by the code a Ctrl-C has to wait for it to end; not measured.

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

Reads `ANTHROPIC_API_KEY` and `ANTHROPIC_BASE_URL` through `ClientConfig::from_lookup`, from the environment, else from `./.env`; it does not set environment variables, since `std::env::set_var` is `unsafe` in Rust 2024. Model text, tool output and approval previews are printed with control characters other than newline and tab, and invisible format characters (`hidden` in `src/main.rs`), replaced by `�`, so a file cannot send escape sequences to the terminal or make a command read differently from what runs; that replacement has no automated test, only the fake-API check. Each input line is one request. When a turn fails with an API error, the turn's messages are dropped from the history so the conversation stays well-formed; side effects of tools that already ran are not undone. Each drop is also written to the session file ([sessions](sessions.md)).
