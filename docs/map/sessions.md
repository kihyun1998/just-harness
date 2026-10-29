# Sessions on disk

How a conversation is kept as a file and resumed (`src/session.rs`, `src/main.rs` `open_session` and `Log`), and why. Prior art read from source: codex's rollout recorder (`codex-rs/rollout/src/recorder.rs`: one `.jsonl` per session, a `SessionMeta` first line, file creation deferred until there is something to write, unparsable lines warned about and skipped on load; `list.rs` filters sessions by the `cwd` in their meta) and Claude Code's own session files (`~/.claude/projects/<folder>/<uuid>.jsonl`, one typed record per line, the file mode 0600 and `~/.claude/projects` 0700, measured on this machine on 2026-10-29 under umask 022).

## The file

`~/.just-harness/sessions/<working folder with each / as ->/<id>.jsonl`, created with the first message, not at start, so starting and quitting leaves no empty session for `--continue` to pick (lens reproduced that). The first line is the `SessionMeta` (`version`, `id`, `created` in Unix seconds, `cwd`, `model`) tagged `"type":"session"`. Each later line is a `message` record, written and synced to the disk (`sync_data`) as the message is added, or a `drop` record. The id is the creation second and 8 random hex digits (`new_id`). The file is mode 0600 and the folders the harness creates 0700, as Claude Code does, since the conversation holds every tool output, an approved `cat .env` included.

`--continue` takes the most recently modified session in the folder whose meta names this working folder; the slug alone is not unique (`/x/y` and `/x-y` share one), which lens reproduced. A session file of a newer `version` than `FORMAT_VERSION` is refused rather than read with its unknown records skipped.

Maintainer's calls (2026-10-29, each shown with its alternatives): the folder-per-working-folder layout under the home folder, not codex's `YYYY/MM/DD` and not a `.just-harness/` inside the project (where a conversation full of tool output could be committed); `--continue` and `--resume <id|path>`, not `--resume` alone.

## Why each record exists

- **`drop`.** The REPL removes a turn from the history on an API error or Ctrl-C (see [agent-loop](agent-loop.md)). Appending messages alone would bring that turn back on resume, so the REPL writes `{"type":"drop","keep":n}` and `load` cuts the history back to `n`.
- **The repair.** A process that ends between an assistant `tool_use` and its results leaves a call with no answer, and the API refuses the next request with a 400 (probed in #2, [agent-loop](agent-loop.md)). `load` answers each such call at the end of the history with `No result: the session ended before this call's result was recorded, so it may or may not have run.` (the call may have run before the process ended) and returns it as `repair`; resuming appends it to the file, so it happens once.
- **The cut.** A call inside the history with no result in the next message (a result line that did not parse) is not repairable in place, so `load` cuts the history at that assistant message and reports `cut_at`; resuming records it as a drop.
- **Cut lines.** A crash mid-write leaves a last line with no line end. `load` skips any line that does not parse and counts it (the REPL says how many), as codex does. `SessionLog::open` writes a line end first when the file does not end with one; without it the first message after resuming was glued to the cut line and lost too, which the fake-API check showed and `appending_after_a_cut_last_line_starts_a_new_line` pins.
- **A failed write** stops all writing to that file for the rest of the session, and the REPL says so; continuing would leave a gap that a later resume reads as a broken history, and a `drop` count that no longer matches the file.

## What is not in the file

The system prompt is rebuilt when the REPL starts, and the `a` allow-for-the-session answers (see [agent-loop](agent-loop.md), Approval) start empty on resume, so a resumed session asks again. A session resumed from another working folder runs its tools in the new one; the REPL warns when `cwd` differs. Paths, ids and errors read from a session file are printed through `printable`.

## How messages reach the file

`run_turn` appends through one function, `append` in `src/agent.rs`, which calls `Observer::on_message` after each message lands in the history; the REPL's observer writes it. The REPL writes the user's own line itself.

Checked on 2026-10-29 against a local fake API with `HOME` in a scratch folder: start and quit (no file), `--continue` with nothing saved, a new session then `--continue` (the next request carried the earlier messages; file 0600, folders 0700), a cancelled turn (dropped, not restored), and `--resume <path>` on a file ending in a cut line after an unanswered call (skipped, warned, answered, next request valid). `main.rs` has no automated test; neither does `sync_data`, nor `latest` skipping an entry whose metadata cannot be read.

## Not built

No listing of sessions, no cleanup of old ones, no locking against two processes writing one file.
