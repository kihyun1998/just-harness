//! Clearing old tool results from the history, and the thinking blocks that
//! clearing invalidates.

use serde_json::Value;

use crate::api::{ContentBlock, MessageParam};

/// What a cleared tool result's content becomes.
pub const CLEARED: &str =
    "[Old tool result cleared to save context. Call the tool again if you need it.]";

/// Stands in for an assistant message whose only blocks were thinking.
const REASONING_CLEARED: &str = "[Earlier reasoning cleared.]";

/// When to clear, and how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClearPolicy {
    /// Clear once the last request's prompt reached this many tokens.
    pub trigger_tokens: u64,
    /// The most recent tool results that are never cleared.
    pub keep_results: usize,
    /// Clear only when at least this many bytes of results would go.
    pub min_cleared_bytes: usize,
}

impl Default for ClearPolicy {
    fn default() -> Self {
        Self {
            trigger_tokens: 80_000,
            keep_results: 3,
            min_cleared_bytes: 40_000,
        }
    }
}

/// What one [`clear_before`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleared {
    /// Tool results replaced with [`CLEARED`].
    pub results: usize,
    /// The size of the content they had.
    pub bytes: usize,
    /// Thinking and redacted thinking blocks removed.
    pub thinking: usize,
}

/// The boundary to pass to [`clear_before`]: the message holding the
/// `keep_results`-th most recent tool result. `None` when the prompt is
/// below the trigger, the history ends with tool results, or too little
/// would be cleared.
pub fn plan(history: &[MessageParam], policy: &ClearPolicy, prompt_tokens: u64) -> Option<usize> {
    if prompt_tokens < policy.trigger_tokens || ends_with_results(history) {
        return None;
    }
    let results: Vec<(usize, &ContentBlock)> = history
        .iter()
        .enumerate()
        .flat_map(|(i, m)| m.content.iter().map(move |b| (i, b)))
        .filter(|(_, b)| b.block_type() == "tool_result")
        .collect();
    let boundary = match results.len().checked_sub(policy.keep_results) {
        Some(n) if n < results.len() => results[n].0,
        Some(_) => history.len(),
        None => return None,
    };
    let bytes: usize = results
        .iter()
        .filter(|(i, _)| *i < boundary)
        .filter_map(|(_, b)| uncleared(b))
        .map(content_bytes)
        .sum();
    (bytes > 0 && bytes >= policy.min_cleared_bytes).then_some(boundary)
}

/// Replaces the content of every tool result in `history[..before]` longer
/// than [`CLEARED`] with it, and removes every thinking block after the first message
/// that changed. A message left empty gets a text block. Applying it again
/// with the same `before` changes nothing.
pub fn clear_before(history: &mut [MessageParam], before: usize) -> Cleared {
    let mut cleared = Cleared::default();
    let mut first_changed = None;
    for (i, message) in history.iter_mut().enumerate().take(before) {
        for block in &mut message.content {
            if block.block_type() != "tool_result" {
                continue;
            }
            if let Some(content) = uncleared(block) {
                cleared.bytes += content_bytes(content);
                cleared.results += 1;
                block.0["content"] = Value::from(CLEARED);
                first_changed.get_or_insert(i);
            }
        }
    }
    let Some(first) = first_changed else {
        return cleared;
    };
    for message in history.iter_mut().skip(first) {
        let had = message.content.len();
        message
            .content
            .retain(|b| !matches!(b.block_type(), "thinking" | "redacted_thinking"));
        cleared.thinking += had - message.content.len();
        if message.content.is_empty() {
            message.content.push(ContentBlock::text(REASONING_CLEARED));
        }
    }
    cleared
}

/// Whether the last message carries tool results, which the model has not
/// replied to yet.
fn ends_with_results(history: &[MessageParam]) -> bool {
    history
        .last()
        .is_some_and(|m| m.content.iter().any(|b| b.block_type() == "tool_result"))
}

/// The content of a tool result worth clearing: not cleared yet, and longer
/// than [`CLEARED`].
fn uncleared(block: &ContentBlock) -> Option<&Value> {
    block
        .0
        .get("content")
        .filter(|c| content_bytes(c) > CLEARED.len())
}

fn content_bytes(content: &Value) -> usize {
    match content {
        Value::String(text) => text.len(),
        other => other.to_string().len(),
    }
}
