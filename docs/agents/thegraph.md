# thegraph

## What this project is

A Rust coding-agent harness in the style of Claude Code, built to learn by building: it calls Claude Haiku 5.5 through the Claude Developer Platform's Messages API and implements context compaction, tool-result clearing, memory, sessions, permissions, hooks, subagents and MCP itself rather than using the API's server-side versions. Running on a local model through Ollama's Messages-compatible endpoint is a later goal.

## References

| Source | Informs | Reached by | Binding |
|---|---|---|---|
| openai/codex `codex-rs/` | how it works | its source tree — raw | example |
| aaif-goose/goose `crates/` | how it works | its source tree — raw | example |
| Anthropic Messages API (platform.claude.com/docs) | how it works | the published docs, raw Markdown at `<page>.md` | binding |
| Ollama Anthropic compatibility (`docs/api/anthropic-compatibility.mdx`) | how it works | its docs in the source tree — raw | binding, for the later local-model goal |
| Claude Code docs (code.claude.com/docs) | how it works | the published docs, raw Markdown at `<page>.md` | example |
