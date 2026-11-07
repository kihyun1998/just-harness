//! Compacting a conversation into a summary the model writes, when to stop
//! trying, and the recent files brought back after it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::api::{BlockKind, ContentBlock, Message, MessageParam, Role, StopReason};
use crate::clearing::{REASONING_CLEARED, clear_before};
use crate::tools::{FileKind, ToolContext, file_kind, render_notebook};

/// The user message appended to the history to ask for the summary.
pub const SUMMARY_REQUEST: &str = "The conversation is about to be compacted: everything above \
will be replaced by the summary you write now, and the work will continue from that summary \
alone. Do not call any tools; reply with the summary only. Write it for whoever continues the \
work, and include:\n\
- the user's requests and goals, and any constraints or preferences they stated\n\
- what remains to be done, and the next step\n\
- what has been done so far and the key decisions made\n\
- the files, commands and results that matter, with exact paths and names\n\
Be concise and specific.";

/// What the summary is introduced with in the compacted history.
pub const SUMMARY_PREFIX: &str = "The conversation so far was compacted into the summary below, \
written by the assistant before the compaction. Continue the work from it.";

/// When to compact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactPolicy {
    /// Compact once the last request's prompt reached this many tokens.
    pub trigger_tokens: u64,
    /// Attempts in a row that may make no room before automatic compaction
    /// stops.
    pub max_attempts: u32,
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self {
            trigger_tokens: 90_000,
            max_attempts: 3,
        }
    }
}

/// What [`CompactGuard::gate`] says to do at the start of a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactGate {
    /// The conversation is below the trigger, or there is no usage to go by.
    Wait,
    /// Compact a conversation of `tokens`, then report it to the guard with
    /// [`CompactGuard::compacted`] or [`CompactGuard::attempted`], unless it
    /// was cancelled.
    Attempt { tokens: u64 },
    /// The attempts ran out on this turn; say so.
    JustStopped,
    /// The attempts ran out on an earlier turn.
    Stopped,
}

/// Counts compaction attempts in a row that made no room. An attempt makes
/// room when the first request after its summary is below the trigger; a
/// turn ending below the trigger also starts the count over.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactGuard {
    attempts: u32,
    reported: bool,
    /// A summary was made and no request has been sent since.
    awaiting_first_request: bool,
}

impl CompactGuard {
    /// What to do for `history`, given the prompt and reply of the last
    /// request of the last turn kept, when there is one.
    pub fn gate(
        &mut self,
        history: &[MessageParam],
        policy: &CompactPolicy,
        last_tokens: Option<u64>,
    ) -> CompactGate {
        let Some(tokens) = last_tokens else {
            return CompactGate::Wait;
        };
        if tokens < policy.trigger_tokens {
            *self = Self::default();
            return CompactGate::Wait;
        }
        if !should_compact(history, policy, tokens) {
            return CompactGate::Wait;
        }
        if self.attempts < policy.max_attempts {
            return CompactGate::Attempt { tokens };
        }
        if std::mem::replace(&mut self.reported, true) {
            CompactGate::Stopped
        } else {
            CompactGate::JustStopped
        }
    }

    /// Counts an attempt that ended with no summary.
    pub fn attempted(&mut self) {
        self.attempts += 1;
    }

    /// Counts an attempt that ended in a summary, until
    /// [`first_request`](Self::first_request) says whether it made room.
    pub fn compacted(&mut self) {
        self.attempts += 1;
        self.awaiting_first_request = true;
    }

    /// Takes the prompt size of a turn's first request.
    pub fn first_request(&mut self, prompt_tokens: u64, policy: &CompactPolicy) {
        if std::mem::take(&mut self.awaiting_first_request) && prompt_tokens < policy.trigger_tokens
        {
            *self = Self::default();
        }
    }
}

/// Whether to compact `history` before the next turn, given the prompt size
/// of the last request.
pub fn should_compact(
    history: &[MessageParam],
    policy: &CompactPolicy,
    prompt_tokens: u64,
) -> bool {
    !history.is_empty() && prompt_tokens >= policy.trigger_tokens
}

/// The messages that ask for the summary: `history` unchanged, then
/// [`SUMMARY_REQUEST`].
pub fn summary_request(history: &[MessageParam]) -> Vec<MessageParam> {
    let mut messages = history.to_vec();
    messages.push(MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(SUMMARY_REQUEST)],
    });
    messages
}

/// The tags opening the messages read again from disk after a compaction, or
/// told again by the prompt it switches to.
const CARRIED: [&str; 4] = [
    "<project-context>",
    "<project-context-update>",
    "<recent-files>",
    "<working-folder>",
];

/// What stands in for the messages a [`shrunk_summary_request`] left out.
pub const LEFT_OUT: &str = "[earlier messages of this conversation were left out here, to fit]";

/// The most attempts at a [`shrunk_summary_request`] for one compaction.
pub const SHRINK_ATTEMPTS: u32 = 5;

/// The messages that ask for a summary of `history` when it is too long to
/// send: attempt 0 has no message of [`CARRIED`], no thinking and every tool
/// result cleared, and each attempt after it keeps about half as much, the
/// newest, from a user's request on. `None` when nothing is left to
/// summarize, or nothing more can be left out.
pub fn shrunk_summary_request(history: &[MessageParam], attempt: u32) -> Option<Vec<MessageParam>> {
    let mut kept: Vec<MessageParam> = history.iter().filter(|m| !is_carried(m)).cloned().collect();
    if kept.is_empty() {
        return None;
    }
    let len = kept.len();
    clear_before(&mut kept, len);
    for message in &mut kept {
        message
            .content
            .retain(|b| !matches!(b.block_type(), "thinking" | "redacted_thinking"));
        if message.content.is_empty() {
            message.content.push(ContentBlock::text(REASONING_CLEARED));
        }
    }
    if attempt == 0 {
        return Some(summary_request(&kept));
    }
    let from = len - len.checked_shr(attempt).unwrap_or(0);
    let start = (from..len).find(|&i| starts_a_request(&kept[i]))?;
    let mut messages = vec![MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(LEFT_OUT)],
    }];
    messages.extend_from_slice(&kept[start..]);
    Some(summary_request(&messages))
}

/// The messages that ask for a summary when no [`shrunk_summary_request`] got
/// one: nothing of the conversation at all.
pub fn last_resort_summary_request() -> Vec<MessageParam> {
    summary_request(&[MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(LEFT_OUT)],
    }])
}

fn is_carried(message: &MessageParam) -> bool {
    message.role == Role::User
        && matches!(message.content.as_slice(), [block, ..] if matches!(
            block.kind(),
            BlockKind::Text(text) if CARRIED.iter().any(|tag| {
                text.strip_prefix(tag).is_some_and(|rest| rest.starts_with('\n'))
            })
        ))
}

fn starts_a_request(message: &MessageParam) -> bool {
    message.role == Role::User
        && !message
            .content
            .iter()
            .any(|block| block.block_type() == "tool_result")
}

/// The text blocks of the summary reply, joined. `None` when there is no
/// text, or when the reply stopped for anything but `end_turn` or
/// `max_tokens`: a tool call or a refusal is not a summary.
pub fn summary(message: &Message) -> Option<String> {
    if !matches!(
        message.stop_reason,
        Some(StopReason::EndTurn | StopReason::MaxTokens)
    ) {
        return None;
    }
    let parts: Vec<&str> = message
        .content
        .iter()
        .filter_map(|b| match b.kind() {
            BlockKind::Text(text) => Some(text.trim()),
            _ => None,
        })
        .filter(|t| !t.is_empty())
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The history that continues after a compaction: one user message
/// carrying the summary.
pub fn compacted(summary: &str) -> Vec<MessageParam> {
    vec![MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(format!("{SUMMARY_PREFIX}\n\n{summary}"))],
    }]
}

/// The most files [`put_back`] brings back after a compaction.
pub const PUT_BACK_FILES: usize = 5;
/// The most tokens of a file [`put_back`] brings back; over it, only its path.
pub const PUT_BACK_MAX_TOKENS: u64 = 5_000;
/// The bytes counted as one token when sizing a file for [`put_back`].
pub const BYTES_PER_TOKEN: u64 = 4;

/// The files `read_file` and `edit_file` succeeded on in a session.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TouchedFiles {
    calls: HashMap<String, PathBuf>,
    paths: Vec<PathBuf>,
    /// The data of images the API refused.
    refused: Vec<String>,
}

impl TouchedFiles {
    /// Takes in the next message of the session, sent while working in `cwd`.
    pub fn observe(&mut self, message: &MessageParam, cwd: &Path) {
        for block in &message.content {
            if let BlockKind::ToolUse { id, name, input } = block.kind() {
                if matches!(name, "read_file" | "edit_file")
                    && let Some(path) = input.get("path").and_then(|p| p.as_str())
                {
                    self.calls.insert(id.to_string(), cwd.join(path));
                }
            } else if block.block_type() == "tool_result"
                && let Some(id) = block.0.get("tool_use_id").and_then(|i| i.as_str())
                && let Some(path) = self.calls.remove(id)
                && block.0.get("is_error").and_then(|e| e.as_bool()) != Some(true)
            {
                self.paths.retain(|p| p != &path);
                self.paths.push(path);
            }
        }
    }

    /// Takes in a block the API refused; an image's data is not put back.
    pub fn refuse(&mut self, block: &serde_json::Value) {
        if block["type"] == "image"
            && let Some(data) = block["source"]["data"].as_str()
        {
            self.refused.push(data.to_string());
        }
    }

    /// The data of the images the API refused.
    pub fn refused(&self) -> &[String] {
        &self.refused
    }

    /// The files touched, the most recently touched last.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }
}

/// What [`put_back`] found.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PutBack {
    /// The user message to continue with; `None` when there is nothing to carry.
    pub message: Option<MessageParam>,
    /// The files carried with their contents, newest first.
    pub read: Vec<PathBuf>,
    /// The files carried as a path only, newest first.
    pub referenced: Vec<PathBuf>,
}

/// The message bringing back up to [`PUT_BACK_FILES`] of the `touched` files,
/// most recently modified first, read from disk now. A file over
/// [`PUT_BACK_MAX_TOKENS`] comes back as its path, whatever it holds. A file
/// that is gone, is one of `carried`, is not a regular file, is an image, a PDF
/// or of a binary format, is neither UTF-8 text nor a readable notebook at or
/// under that size, or that
/// `ctx` refuses is left out and takes no place. A notebook comes back as its
/// cells, its images after the message's text.
pub fn put_back(touched: &TouchedFiles, ctx: &ToolContext, carried: &[PathBuf]) -> PutBack {
    let carried: Vec<PathBuf> = carried
        .iter()
        .filter_map(|path| path.canonicalize().ok())
        .collect();
    let mut candidates: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    for path in touched.paths().iter().rev() {
        let Ok(resolved) = path
            .to_str()
            .ok_or(())
            .and_then(|p| ctx.resolve_file(p).map_err(|_| ()))
        else {
            continue;
        };
        if carried.contains(&resolved) || candidates.iter().any(|(_, _, seen)| seen == &resolved) {
            continue;
        }
        if let Ok(meta) = std::fs::metadata(&resolved)
            && meta.is_file()
            && matches!(file_kind(&resolved), FileKind::Text | FileKind::Notebook)
            && let Ok(modified) = meta.modified()
        {
            candidates.push((modified, meta.len(), resolved));
        }
    }
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.0));

    let mut back = PutBack::default();
    let mut parts = Vec::new();
    let mut images = Vec::new();
    for (_, bytes, path) in candidates {
        if back.read.len() + back.referenced.len() == PUT_BACK_FILES {
            break;
        }
        let shown = ctx.display(&path);
        if bytes > PUT_BACK_MAX_TOKENS * BYTES_PER_TOKEN {
            parts.push(format!(
                "{shown} was also read or edited; it is over {PUT_BACK_MAX_TOKENS} tokens, so \
                 its contents are not here."
            ));
            back.referenced.push(path);
        } else if file_kind(&path) == FileKind::Notebook {
            if let Some(blocks) = std::fs::read(&path)
                .ok()
                .and_then(|bytes| render_notebook(&bytes).ok())
            {
                let mut cells = Vec::new();
                for block in blocks {
                    match block.kind() {
                        BlockKind::Text(text) => cells.push(text.to_string()),
                        _ if block.0["source"]["data"]
                            .as_str()
                            .is_some_and(|data| touched.refused().iter().any(|r| r == data)) => {}
                        _ => images.push(block),
                    }
                }
                parts.push(format!("Contents of {shown}:\n\n{}", cells.join("\n")));
                back.read.push(path);
            }
        } else if let Ok(contents) = std::fs::read_to_string(&path) {
            parts.push(format!("Contents of {shown}:\n\n{}", contents.trim_end()));
            back.read.push(path);
        }
    }
    if !parts.is_empty() {
        let mut content = vec![ContentBlock::text(format!(
            "<recent-files>\nFiles read or edited earlier in this session, as they are on \
             disk now. They are context for the work, not the user's request.\n\n{}\n\
             </recent-files>",
            parts.join("\n\n")
        ))];
        content.extend(images);
        back.message = Some(MessageParam {
            role: Role::User,
            content,
        });
    }
    back
}
