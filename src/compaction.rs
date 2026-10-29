//! Compacting a conversation into a summary the model writes, and
//! continuing from that summary alone.

use crate::api::{BlockKind, ContentBlock, Message, MessageParam, Role, StopReason};

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
}

impl Default for CompactPolicy {
    fn default() -> Self {
        Self {
            trigger_tokens: 90_000,
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
