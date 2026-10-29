use just_harness::api::{ContentBlock, MessageParam, Role};
use just_harness::clearing::{CLEARED, ClearPolicy, clear_before, plan};
use serde_json::{Value, json};

fn user(text: &str) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }
}

fn thinking(signature: &str) -> ContentBlock {
    ContentBlock(json!({ "type": "thinking", "thinking": "", "signature": signature }))
}

fn call(id: &str, signature: &str) -> MessageParam {
    MessageParam {
        role: Role::Assistant,
        content: vec![
            thinking(signature),
            ContentBlock(
                json!({ "type": "tool_use", "id": id, "name": "read_file", "input": { "path": "a" } }),
            ),
        ],
    }
}

fn result(id: &str, output: &str) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::tool_result(id, output, false)],
    }
}

fn answer(signature: &str, text: &str) -> MessageParam {
    MessageParam {
        role: Role::Assistant,
        content: vec![thinking(signature), ContentBlock::text(text)],
    }
}

fn result_content(message: &MessageParam) -> &Value {
    &message.content[0].0["content"]
}

fn types(message: &MessageParam) -> Vec<&str> {
    message.content.iter().map(|b| b.block_type()).collect()
}

const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const C: &str =
    "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc";

/// Two user turns: the first reads two files, the second one more.
fn conversation() -> Vec<MessageParam> {
    vec![
        user("read a and b"),       // 0
        call("t1", "s1"),           // 1
        result("t1", A),            // 2
        call("t2", "s2"),           // 3
        result("t2", B),            // 4
        answer("s3", "Read both."), // 5
        user("now c"),              // 6
        call("t3", "s4"),           // 7
        result("t3", C),            // 8
        answer("s5", "Read c."),    // 9
    ]
}

#[test]
fn results_before_the_boundary_are_replaced_and_later_ones_kept() {
    let mut history = conversation();
    let cleared = clear_before(&mut history, 4);

    assert_eq!(result_content(&history[2]), CLEARED);
    assert_eq!(result_content(&history[4]), B);
    assert_eq!(result_content(&history[8]), C);
    assert_eq!(history[2].content[0].0["tool_use_id"], "t1");
    assert_eq!(cleared.results, 1);
    assert_eq!(cleared.bytes, A.len());
}

#[test]
fn a_result_no_longer_than_the_placeholder_is_left_as_it_is() {
    let short = &A[..CLEARED.len()];
    let mut history = conversation();
    history[2] = result("t1", short);
    let cleared = clear_before(&mut history, 4);

    assert_eq!(result_content(&history[2]), short);
    assert_eq!(cleared, Default::default());
    assert_eq!(types(&history[3]), ["thinking", "tool_use"]);
    assert_eq!(plan(&history, &policy(1000, 1, 1), 1000), Some(8));
    assert_eq!(plan(&history, &policy(1000, 1, B.len() + 1), 1000), None);
}

#[test]
fn an_error_result_stays_marked_as_an_error() {
    let mut history = vec![
        user("go"),
        call("t1", "s1"),
        MessageParam {
            role: Role::User,
            content: vec![ContentBlock::tool_result("t1", A, true)],
        },
        answer("s2", "Failed."),
    ];
    clear_before(&mut history, 3);
    assert_eq!(history[2].content[0].0["is_error"], true);
    assert_eq!(result_content(&history[2]), CLEARED);
}

#[test]
fn thinking_after_the_first_cleared_result_is_removed_and_earlier_thinking_kept() {
    let mut history = conversation();
    history[1].content.push(ContentBlock(
        json!({ "type": "redacted_thinking", "data": "x" }),
    ));
    history[3].content.insert(
        0,
        ContentBlock(json!({ "type": "redacted_thinking", "data": "y" })),
    );
    let cleared = clear_before(&mut history, 4);

    assert_eq!(
        types(&history[1]),
        ["thinking", "tool_use", "redacted_thinking"]
    );
    assert_eq!(types(&history[3]), ["tool_use"]);
    assert_eq!(types(&history[5]), ["text"]);
    assert_eq!(types(&history[7]), ["tool_use"]);
    assert_eq!(types(&history[9]), ["text"]);
    assert_eq!(cleared.thinking, 5);
}

#[test]
fn clearing_again_changes_nothing_and_keeps_thinking_made_since() {
    let mut history = conversation();
    clear_before(&mut history, 4);
    history.push(user("and d"));
    history.push(answer("s6", "Done."));
    let before = history.clone();

    let cleared = clear_before(&mut history, 4);

    assert_eq!(history, before);
    assert_eq!(
        (cleared.results, cleared.bytes, cleared.thinking),
        (0, 0, 0)
    );
}

#[test]
fn a_reply_left_with_no_blocks_gets_a_text_block() {
    let mut history = vec![
        user("go"),
        call("t1", "s1"),
        result("t1", A),
        MessageParam {
            role: Role::Assistant,
            content: vec![thinking("s2")],
        },
    ];
    clear_before(&mut history, 3);
    assert_eq!(types(&history[3]), ["text"]);
    assert!(!history[3].content[0].0["text"].as_str().unwrap().is_empty());
}

#[test]
fn a_result_given_as_blocks_is_cleared_and_counted_by_its_json() {
    let blocks = json!([{ "type": "text", "text": A }]);
    let mut history = vec![
        user("go"),
        call("t1", "s1"),
        MessageParam {
            role: Role::User,
            content: vec![ContentBlock(
                json!({ "type": "tool_result", "tool_use_id": "t1", "content": blocks }),
            )],
        },
        answer("s2", "ok"),
    ];
    let cleared = clear_before(&mut history, 3);
    assert_eq!(result_content(&history[2]), CLEARED);
    assert_eq!(cleared.bytes, blocks.to_string().len());
}

fn policy(trigger: u64, keep: usize, min: usize) -> ClearPolicy {
    ClearPolicy {
        trigger_tokens: trigger,
        keep_results: keep,
        min_cleared_bytes: min,
    }
}

#[test]
fn nothing_is_planned_below_the_trigger() {
    assert_eq!(plan(&conversation(), &policy(1000, 1, 1), 999), None);
}

#[test]
fn the_boundary_keeps_the_most_recent_results() {
    let history = conversation();
    assert_eq!(plan(&history, &policy(1000, 1, 1), 1000), Some(8));
    assert_eq!(plan(&history, &policy(1000, 2, 1), 1000), Some(4));
    assert_eq!(plan(&history, &policy(1000, 3, 1), 1000), None);
}

#[test]
fn keeping_no_results_clears_every_one() {
    assert_eq!(plan(&conversation(), &policy(1000, 0, 1), 1000), Some(10));
}

#[test]
fn nothing_is_planned_when_too_little_would_be_cleared() {
    let history = conversation();
    // Keeping one result leaves A and B to clear.
    let both = A.len() + B.len();
    assert_eq!(plan(&history, &policy(1000, 1, both + 1), 1000), None);
    assert_eq!(plan(&history, &policy(1000, 1, both), 1000), Some(8));
}

#[test]
fn results_already_cleared_do_not_count_toward_the_minimum() {
    let mut history = conversation();
    clear_before(&mut history, 4);
    // Left to clear when keeping one: B.
    assert_eq!(plan(&history, &policy(1000, 1, B.len() + 1), 1000), None);
    assert_eq!(plan(&history, &policy(1000, 1, B.len()), 1000), Some(8));
}

#[test]
fn nothing_is_planned_while_results_the_model_has_not_seen_end_the_history() {
    let mut history = conversation();
    history.truncate(9);
    assert_eq!(plan(&history, &policy(1000, 1, 1), 1000), None);
    history.push(answer("s5", "Read c."));
    assert_eq!(plan(&history, &policy(1000, 1, 1), 1000), Some(8));
}
