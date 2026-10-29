use just_harness::api::{ContentBlock, Message, MessageParam, Role, StopReason, Usage};
use just_harness::compaction::{
    CompactPolicy, SUMMARY_PREFIX, SUMMARY_REQUEST, compacted, should_compact, summary,
    summary_request,
};
use serde_json::json;

fn user(text: &str) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}

fn reply(blocks: Vec<ContentBlock>) -> Message {
    reply_stopping(blocks, Some(StopReason::EndTurn))
}

fn reply_stopping(blocks: Vec<ContentBlock>, stop_reason: Option<StopReason>) -> Message {
    Message {
        id: "msg_1".into(),
        model: "claude-haiku-5-5".into(),
        content: blocks,
        stop_reason,
        stop_sequence: None,
        stop_details: None,
        usage: Usage::default(),
    }
}

fn texts(message: &MessageParam) -> Vec<&str> {
    message
        .content
        .iter()
        .map(|b| b.0["text"].as_str().unwrap_or(""))
        .collect()
}

#[test]
fn compaction_waits_for_the_trigger_and_a_history() {
    let policy = CompactPolicy {
        trigger_tokens: 1000,
    };
    let history = vec![user("hi")];
    assert!(!should_compact(&history, &policy, 999));
    assert!(should_compact(&history, &policy, 1000));
    assert!(!should_compact(&[], &policy, 5000));
}

#[test]
fn the_summary_request_is_the_history_with_one_user_message_after_it() {
    let history = vec![user("first"), user("second")];
    let request = summary_request(&history);
    assert_eq!(request[..2], history[..]);
    assert_eq!(request.len(), 3);
    assert_eq!(request[2].role, Role::User);
    assert_eq!(texts(&request[2]), [SUMMARY_REQUEST]);
}

#[test]
fn the_summary_is_the_replys_text_without_thinking_or_tool_calls() {
    let message = reply(vec![
        ContentBlock(
            json!({ "type": "thinking", "thinking": "Let me recall.", "signature": "c2ln" }),
        ),
        ContentBlock::text("Goal: fix the parser."),
        ContentBlock(json!({ "type": "tool_use", "id": "t1", "name": "shell", "input": {} })),
        ContentBlock::text("Next: run the tests."),
    ]);
    assert_eq!(
        summary(&message).as_deref(),
        Some("Goal: fix the parser.\n\nNext: run the tests.")
    );
}

#[test]
fn only_a_finished_reply_or_one_cut_at_max_tokens_is_a_summary() {
    let text = || vec![ContentBlock::text("Let me check the log first.")];
    let cut = reply_stopping(text(), Some(StopReason::MaxTokens));
    assert_eq!(
        summary(&cut).as_deref(),
        Some("Let me check the log first.")
    );
    for stop in [
        Some(StopReason::ToolUse),
        Some(StopReason::Refusal),
        Some(StopReason::PauseTurn),
        Some(StopReason::Other("new".into())),
        None,
    ] {
        assert_eq!(
            summary(&reply_stopping(text(), stop.clone())),
            None,
            "{stop:?}"
        );
    }
}

#[test]
fn a_reply_with_no_text_gives_no_summary() {
    let thinking_only = reply(vec![ContentBlock(
        json!({ "type": "thinking", "thinking": "", "signature": "c2ln" }),
    )]);
    assert_eq!(summary(&thinking_only), None);
    assert_eq!(summary(&reply(vec![ContentBlock::text("  \n ")])), None);
}

#[test]
fn the_compacted_history_is_one_user_message_carrying_the_summary() {
    let history = compacted("Goal: fix the parser.");
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].role, Role::User);
    assert_eq!(
        texts(&history[0]),
        [format!("{SUMMARY_PREFIX}\n\nGoal: fix the parser.").as_str()]
    );
}
