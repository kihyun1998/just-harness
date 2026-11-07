//! Clearing old tool results from the history, replacing a block the API
//! refused, and the thinking blocks either invalidates.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::api::{ContentBlock, MessageParam, Role};

/// What a cleared tool result's content becomes.
pub const CLEARED: &str =
    "[Old tool result cleared to save context. Call the tool again if you need it.]";

/// Stands in for an assistant message whose only blocks were thinking.
pub const REASONING_CLEARED: &str = "[Earlier reasoning cleared.]";

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

/// The most bytes an `image` block's data counts for.
pub const MAX_IMAGE_BYTES: usize = 19_136;

/// What one [`clear_before`] changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cleared {
    /// Tool results replaced with [`CLEARED`].
    pub results: usize,
    /// The size of the content they had, images counted as [`MAX_IMAGE_BYTES`]
    /// at most.
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
    cleared.thinking = remove_thinking_from(history, first);
    cleared
}

/// Removes every thinking block from `history[first..]`, and returns how many
/// there were. A message left empty gets a text block.
fn remove_thinking_from(history: &mut [MessageParam], first: usize) -> usize {
    let mut removed = 0;
    for message in history.iter_mut().skip(first) {
        let had = message.content.len();
        message
            .content
            .retain(|b| !matches!(b.block_type(), "thinking" | "redacted_thinking"));
        removed += had - message.content.len();
        if message.content.is_empty() {
            message.content.push(ContentBlock::text(REASONING_CLEARED));
        }
    }
    removed
}

/// An `image` or `document` block taken out of the history, and the text put
/// in its place.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovedBlock {
    /// The message that held it.
    pub message: usize,
    /// Its place in that message's content.
    pub block: usize,
    /// Its place in a tool result's content, when one held it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nested: Option<usize>,
    pub text: String,
    /// The block taken out; not recorded.
    #[serde(skip)]
    pub old: Option<Value>,
}

/// What stands in for a block of `kind` the API could not process, for `reason`.
pub fn removed_text(kind: &str, reason: &str) -> String {
    let (title, noun) = if kind == "image" {
        ("Image", "image")
    } else {
        ("Document", "document")
    };
    format!(
        "[{title} removed: the API could not process this {noun} ({reason}). The file may be \
         unsupported or corrupt; do not retry reading it. If you need to inspect it, use a shell \
         command instead.]"
    )
}

/// Puts `text` in the place of the block of `kind` at `place`, or of the
/// newest block of `kind` in a user message when `place` holds none, as
/// [`replace_block`] does. `None` when there is none.
pub fn remove_refused_block(
    history: &mut [MessageParam],
    kind: &str,
    place: Option<(usize, usize, Option<usize>)>,
    text: &str,
) -> Option<RemovedBlock> {
    let found = place
        .filter(|&place| holds(history, place, kind))
        .or_else(|| newest(history, kind))?;
    let mut removed = RemovedBlock {
        message: found.0,
        block: found.1,
        nested: found.2,
        text: text.to_string(),
        old: None,
    };
    removed.old = replace_block(history, &removed);
    Some(removed)
}

/// [`remove_refused_block`] with no place named.
pub fn remove_newest_block(
    history: &mut [MessageParam],
    kind: &str,
    text: &str,
) -> Option<RemovedBlock> {
    remove_refused_block(history, kind, None, text)
}

fn holds(
    history: &[MessageParam],
    (message, block, nested): (usize, usize, Option<usize>),
    kind: &str,
) -> bool {
    let Some(found) = history
        .get(message)
        .filter(|m| m.role == Role::User)
        .and_then(|m| m.content.get(block))
    else {
        return false;
    };
    match nested {
        Some(n) => found
            .0
            .get("content")
            .and_then(|c| c.get(n))
            .is_some_and(|b| b["type"] == kind),
        None => found.block_type() == kind,
    }
}

fn newest(history: &[MessageParam], kind: &str) -> Option<(usize, usize, Option<usize>)> {
    history
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, message)| message.role == Role::User)
        .find_map(|(m, message)| {
            message
                .content
                .iter()
                .enumerate()
                .rev()
                .find_map(|(b, block)| {
                    if block.block_type() == kind {
                        return Some((m, b, None));
                    }
                    let nested = block.0.get("content")?.as_array()?;
                    let n = nested.iter().rposition(|inner| inner["type"] == kind)?;
                    Some((m, b, Some(n)))
                })
        })
}

/// Applies `removed` to `history`, removes every thinking block after the
/// message it changes, and returns the block it took out. A place that is not
/// there changes nothing.
pub fn replace_block(history: &mut [MessageParam], removed: &RemovedBlock) -> Option<Value> {
    let block = history
        .get_mut(removed.message)
        .and_then(|message| message.content.get_mut(removed.block))?;
    let text = json!({ "type": "text", "text": removed.text });
    let place = match removed.nested {
        Some(nested) => block
            .0
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .and_then(|inner| inner.get_mut(nested)),
        None => Some(&mut block.0),
    };
    let old = std::mem::replace(place?, text);
    remove_thinking_from(history, removed.message + 1);
    Some(old)
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
        other => other.to_string().len() - image_excess(other),
    }
}

/// How much of `content`'s JSON is image data past [`MAX_IMAGE_BYTES`] each.
fn image_excess(content: &Value) -> usize {
    content
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "image")
        .filter_map(|block| block["source"]["data"].as_str())
        .map(|data| data.len().saturating_sub(MAX_IMAGE_BYTES))
        .sum()
}
