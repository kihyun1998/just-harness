# Measuring without paid calls

Claude Code's behaviour is the target (`docs/agents/thegraph.md`), and its docs do not cover everything: #17 found that every resume re-reads the instruction files, where the docs describe only `/cd`. The installed `claude` can be measured for free by pointing it at a local fake API. The same fake API drives this harness's REPL, which has no automated test yet (#18).

## Rules

- Never run a measurement against the real API by accident: set `ANTHROPIC_BASE_URL` on the same command.
- Never use the default Claude Code config folder or this harness's real home. A measurement gets a scratch `CLAUDE_CONFIG_DIR` and `HOME`.
- A live call costs money, so the maintainer approves it first, with an amount. The key in `.env` got `401 invalid x-api-key` on 2026-11-05 and has to be replaced before any live call or new fixture recording.
- Say which it was when writing a result down: measured here, reported by a review pass, or read from the docs. Only `-p` mode was measured directly so far.

## The fake API

`examples/fake_api.py` logs every request and answers by the last user line. Its docstring lists the words it reacts to. The flags are `--port`, `--log`, `--limit` (refuse past that many bytes of messages, as "prompt is too long"), `--report-tokens` and `--refuse-summary-with-results`.

```sh
python3 examples/fake_api.py --port 18765 --log /tmp/x/requests.jsonl --limit 30000 &
```

Each log line is `{"path", "size", "over", "body"}`, so the request is there to compare.

## Measuring Claude Code

```sh
CLAUDE_CONFIG_DIR=/tmp/x/config \
ANTHROPIC_API_KEY=sk-ant-fake \
ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1 \
claude -p --session-id <uuid> "first line" </dev/null

claude -p --resume <uuid> "second line" </dev/null    # same variables, from any folder
```

Read `messages` and `system` in the log. To measure the home folder's own files, set `HOME` to a scratch folder and unset `CLAUDE_CONFIG_DIR`. For worktrees, `git worktree add --orphan -b <name> <path>` needs no commit in the scratch repository.

## Driving this harness

```sh
cd /tmp/x/project
(echo "read a.txt"; sleep 1; echo "second line"; sleep 1) |
  HOME=/tmp/x/home ANTHROPIC_API_KEY=x ANTHROPIC_BASE_URL=http://127.0.0.1:18765 \
  /path/to/target/debug/just-harness
```

## Traps met

- **Lines typed ahead are dropped.** The approval prompt discards what arrived before it (`docs/map/agent-loop.md`), and the first start can take seconds on a slow disk. Pause about 4 seconds before an answer such as `y`.
- **zsh does not split `$FLAGS`.** `"$BIN" $FLAGS` with `--resume <path>` passes one argument and fails with the usage line.
- **`timeout` is not on macOS.** `timeout 110 cargo build 2>&1 | grep ...` prints nothing and looks like a clean build.
- **Mutation runs rebuild the binary.** `cargo test` builds `target/debug/just-harness` too, so do not drive the REPL while a mutation is applied.
- **A pipe hides a failure.** Run the checks bare at the end: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- **A checker that cannot name a folder may block a command.** A hook on `git` commands refused `git -C <path>` and `cd` into a scratch folder in one command. Keep such setup in a script file.
