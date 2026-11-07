use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use just_harness::api::{ContentBlock, Message, MessageParam, Role, StopReason, Usage};
use just_harness::compaction::{
    CompactGate, CompactGuard, CompactPolicy, LEFT_OUT, PutBack, SUMMARY_PREFIX, SUMMARY_REQUEST,
    TouchedFiles, compacted, last_resort_summary_request, put_back, should_compact,
    shrunk_summary_request, summary, summary_request,
};
use just_harness::tools::ToolContext;
use serde_json::json;

const POLICY: CompactPolicy = CompactPolicy {
    trigger_tokens: 1000,
    max_attempts: 3,
};

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
    let history = vec![user("hi")];
    assert!(!should_compact(&history, &POLICY, 999));
    assert!(should_compact(&history, &POLICY, 1000));
    assert!(!should_compact(&[], &POLICY, 5000));
}

#[test]
fn the_guard_allows_three_attempts_then_stops_and_says_so_once() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..3 {
        assert_eq!(
            guard.gate(&history, &POLICY, Some(1000)),
            CompactGate::Attempt { tokens: 1000 }
        );
        guard.attempted();
    }
    assert_eq!(
        guard.gate(&history, &POLICY, Some(1000)),
        CompactGate::JustStopped
    );
    assert_eq!(
        guard.gate(&history, &POLICY, Some(1000)),
        CompactGate::Stopped
    );
    assert_eq!(
        guard.gate(&history, &POLICY, Some(5000)),
        CompactGate::Stopped
    );
}

#[test]
fn a_gate_that_is_not_followed_by_an_attempt_does_not_count() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..5 {
        assert_eq!(
            guard.gate(&history, &POLICY, Some(1000)),
            CompactGate::Attempt { tokens: 1000 }
        );
    }
}

#[test]
fn a_turn_ending_below_the_trigger_starts_the_count_over() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..3 {
        guard.attempted();
    }
    assert_eq!(
        guard.gate(&history, &POLICY, Some(1000)),
        CompactGate::JustStopped
    );

    assert_eq!(guard.gate(&history, &POLICY, Some(999)), CompactGate::Wait);

    for _ in 0..3 {
        assert_eq!(
            guard.gate(&history, &POLICY, Some(1000)),
            CompactGate::Attempt { tokens: 1000 }
        );
        guard.attempted();
    }
    assert_eq!(
        guard.gate(&history, &POLICY, Some(1000)),
        CompactGate::JustStopped,
        "a new stop is reported again"
    );
}

#[test]
fn a_compaction_that_made_room_starts_the_count_over_however_the_turn_ends() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..5 {
        assert_eq!(
            guard.gate(&history, &POLICY, Some(5000)),
            CompactGate::Attempt { tokens: 5000 }
        );
        guard.compacted();
        guard.first_request(999, &POLICY);
    }
}

#[test]
fn a_compaction_that_left_the_conversation_at_the_trigger_counts() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..3 {
        assert_eq!(
            guard.gate(&history, &POLICY, Some(5000)),
            CompactGate::Attempt { tokens: 5000 }
        );
        guard.compacted();
        guard.first_request(1000, &POLICY);
    }
    assert_eq!(
        guard.gate(&history, &POLICY, Some(5000)),
        CompactGate::JustStopped
    );
}

#[test]
fn only_the_first_request_after_a_compaction_says_whether_it_made_room() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..3 {
        guard.compacted();
        guard.first_request(1000, &POLICY);
        // A later turn's first request, small, is not the one that counts.
        guard.first_request(1, &POLICY);
    }
    assert_eq!(
        guard.gate(&history, &POLICY, Some(5000)),
        CompactGate::JustStopped
    );
}

#[test]
fn a_failed_summary_is_not_undone_by_the_next_turns_first_request() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..3 {
        guard.attempted();
        guard.first_request(1, &POLICY);
    }
    assert_eq!(
        guard.gate(&history, &POLICY, Some(5000)),
        CompactGate::JustStopped
    );
}

#[test]
fn a_turn_with_no_usage_neither_compacts_nor_starts_the_count_over() {
    let history = vec![user("hi")];
    let mut guard = CompactGuard::default();
    for _ in 0..3 {
        guard.attempted();
    }
    assert_eq!(guard.gate(&history, &POLICY, None), CompactGate::Wait);
    assert_eq!(
        guard.gate(&history, &POLICY, Some(1000)),
        CompactGate::JustStopped
    );
}

#[test]
fn an_empty_history_is_not_compacted() {
    let mut guard = CompactGuard::default();
    assert_eq!(guard.gate(&[], &POLICY, Some(1000)), CompactGate::Wait);
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

fn call(id: &str, name: &str, path: &str) -> MessageParam {
    MessageParam {
        role: Role::Assistant,
        content: vec![ContentBlock(
            json!({ "type": "tool_use", "id": id, "name": name, "input": { "path": path } }),
        )],
    }
}

fn result(id: &str, is_error: bool) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::tool_result(id, "output", is_error)],
    }
}

fn touched_by(calls: &[(&str, &str, bool)], cwd: &Path) -> TouchedFiles {
    let mut touched = TouchedFiles::default();
    for (i, (name, path, is_error)) in calls.iter().enumerate() {
        let id = format!("call_{i}");
        touched.observe(&call(&id, name, path), cwd);
        touched.observe(&result(&id, *is_error), cwd);
    }
    touched
}

#[test]
fn only_a_successful_read_or_edit_counts_as_touching_a_file() {
    let cwd = Path::new("/work");
    let touched = touched_by(
        &[
            ("read_file", "read.rs", false),
            ("edit_file", "edited.rs", false),
            ("read_file", "refused.rs", true),
            ("edit_file", "denied.rs", true),
            ("shell", "shell.rs", false),
            ("search", "search.rs", false),
        ],
        cwd,
    );
    assert_eq!(
        touched.paths(),
        [
            PathBuf::from("/work/read.rs"),
            PathBuf::from("/work/edited.rs")
        ]
    );
}

#[test]
fn a_call_with_no_result_does_not_count() {
    let cwd = Path::new("/work");
    let mut touched = TouchedFiles::default();
    touched.observe(&call("a", "edit_file", "cut.rs"), cwd);
    assert!(touched.paths().is_empty());
}

#[test]
fn a_relative_path_belongs_to_the_folder_it_was_called_in() {
    let mut touched = TouchedFiles::default();
    touched.observe(&call("a", "read_file", "x.rs"), Path::new("/first"));
    touched.observe(&result("a", false), Path::new("/first"));
    touched.observe(&call("b", "read_file", "/abs/y.rs"), Path::new("/second"));
    touched.observe(&result("b", false), Path::new("/second"));
    assert_eq!(
        touched.paths(),
        [PathBuf::from("/first/x.rs"), PathBuf::from("/abs/y.rs")]
    );
}

/// Writes `name` under `dir`, last modified `age` seconds before a fixed time.
fn write_aged(dir: &Path, name: &str, contents: impl AsRef<[u8]>, age: u64) {
    let path = dir.join(name);
    std::fs::write(&path, contents).unwrap();
    let time = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000_000 - age);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(time)
        .unwrap();
}

fn message_text(message: &MessageParam) -> &str {
    message.content[0].0["text"].as_str().unwrap()
}

#[test]
fn nothing_touched_puts_nothing_back() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = ToolContext::new(dir.path()).unwrap();
    let back = put_back(&TouchedFiles::default(), &ctx, &[]);
    assert_eq!(back, PutBack::default());
}

#[test]
fn the_five_most_recently_modified_files_come_back_newest_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // Touched in the order a..f, modified in a different order.
    for (name, age) in [
        ("a", 30),
        ("b", 10),
        ("c", 50),
        ("d", 20),
        ("e", 60),
        ("f", 40),
    ] {
        write_aged(&root, name, format!("contents of {name}\n"), age);
    }
    let touched = touched_by(
        &[
            ("read_file", "a", false),
            ("read_file", "b", false),
            ("edit_file", "c", false),
            ("read_file", "d", false),
            ("edit_file", "e", false),
            ("read_file", "f", false),
        ],
        &root,
    );
    let ctx = ToolContext::new(&root).unwrap();
    let back = put_back(&touched, &ctx, &[]);

    let expected: Vec<PathBuf> = ["b", "d", "a", "f", "c"]
        .iter()
        .map(|n| root.join(n))
        .collect();
    assert_eq!(back.read, expected);
    assert!(back.referenced.is_empty());
    let message = back.message.unwrap();
    assert_eq!(message.role, Role::User);
    let text = message_text(&message);
    let at = |name: &str| {
        text.find(&format!("Contents of {name}:\n\ncontents of {name}"))
            .unwrap()
    };
    assert!(at("b") < at("d") && at("d") < at("a") && at("a") < at("f") && at("f") < at("c"));
    assert!(!text.contains("contents of e"), "the oldest is left out");
}

#[test]
fn a_file_over_5000_tokens_comes_back_as_its_path_and_still_takes_a_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    // 5,000 tokens at 4 bytes each is 20,000 bytes.
    write_aged(&root, "fits", "f".repeat(20_000), 10);
    write_aged(&root, "big", "b".repeat(20_001), 5);
    for (name, age) in [("c", 20), ("d", 30), ("e", 40), ("old", 50)] {
        write_aged(&root, name, name, age);
    }
    let touched = touched_by(
        &["old", "e", "d", "c", "fits", "big"].map(|n| ("read_file", n, false)),
        &root,
    );
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);

    assert_eq!(back.referenced, [root.join("big")]);
    assert_eq!(back.read, ["fits", "c", "d", "e"].map(|n| root.join(n)));
    let message = back.message.unwrap();
    let text = message_text(&message);
    assert!(text.contains(&"f".repeat(20_000)));
    assert!(!text.contains("bbb"), "the big file's contents stay out");
    assert!(text.contains("big"), "the big file's path is given");
    assert!(!text.contains("Contents of old"), "five places in all");
}

#[test]
fn a_file_that_is_gone_or_may_not_be_read_is_left_out() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let outside_root = outside.path().canonicalize().unwrap();
    write_aged(&root, "kept", "kept text", 10);
    write_aged(&root, ".env", "SECRET=1", 1);
    write_aged(&root, "binary", [0xff, 0xfe, 0x00], 2);
    std::fs::create_dir(root.join("folder")).unwrap();
    write_aged(&outside_root, "elsewhere", "elsewhere text", 3);
    std::os::unix::fs::symlink(outside_root.join("elsewhere"), root.join("link")).unwrap();
    let elsewhere = outside_root.join("elsewhere");
    let touched = touched_by(
        &[
            ("read_file", "kept", false),
            ("read_file", ".env", false),
            ("read_file", "binary", false),
            ("read_file", "folder", false),
            ("read_file", "gone", false),
            ("read_file", "link", false),
            ("read_file", elsewhere.to_str().unwrap(), false),
        ],
        &root,
    );
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);

    assert_eq!(back.read, [root.join("kept")]);
    assert!(back.referenced.is_empty());
    let message = back.message.unwrap();
    let text = message_text(&message);
    assert!(text.contains("kept text"));
    assert!(!text.contains("SECRET") && !text.contains("elsewhere"));
}

#[test]
fn a_file_touched_under_two_names_comes_back_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "same", "same text", 10);
    std::os::unix::fs::symlink(root.join("same"), root.join("alias")).unwrap();
    let touched = touched_by(
        &[("read_file", "same", false), ("edit_file", "alias", false)],
        &root,
    );
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);
    assert_eq!(back.read, [root.join("same")]);
}

#[test]
fn a_file_in_a_folder_allowed_beside_the_working_folder_comes_back() {
    let dir = tempfile::tempdir().unwrap();
    let memory = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let memory_root = memory.path().canonicalize().unwrap();
    write_aged(&memory_root, "note.md", "a note", 10);
    let note = memory_root.join("note.md");
    let touched = touched_by(&[("edit_file", note.to_str().unwrap(), false)], &root);
    let ctx = ToolContext::new(&root)
        .unwrap()
        .allow(&memory_root)
        .unwrap();
    assert_eq!(put_back(&touched, &ctx, &[]).read, [note]);
}

#[test]
fn a_path_touched_again_is_listed_once_as_the_latest() {
    let touched = touched_by(
        &[
            ("read_file", "a", false),
            ("read_file", "b", false),
            ("edit_file", "a", false),
        ],
        Path::new("/work"),
    );
    assert_eq!(
        touched.paths(),
        [PathBuf::from("/work/b"), PathBuf::from("/work/a")]
    );
}

#[test]
fn files_modified_at_the_same_moment_come_back_latest_touched_first() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "first", "1", 10);
    write_aged(&root, "second", "2", 10);
    let touched = touched_by(
        &[
            ("read_file", "first", false),
            ("read_file", "second", false),
        ],
        &root,
    );
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);
    assert_eq!(back.read, [root.join("second"), root.join("first")]);
}

#[test]
fn a_file_left_out_takes_no_place_among_the_five() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "binary", [0xff, 0xfe, 0x00], 1);
    for (name, age) in [("a", 10), ("b", 20), ("c", 30), ("d", 40), ("e", 50)] {
        write_aged(&root, name, name, age);
    }
    let touched = touched_by(
        &["a", "b", "c", "d", "e", "binary"].map(|n| ("read_file", n, false)),
        &root,
    );
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);
    assert_eq!(back.read, ["a", "b", "c", "d", "e"].map(|n| root.join(n)));
}

#[test]
fn something_that_is_not_a_regular_file_is_not_opened() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let status = std::process::Command::new("mkfifo")
        .arg(root.join("pipe"))
        .status()
        .unwrap();
    assert!(status.success());
    let touched = touched_by(&[("read_file", "pipe", false)], &root);
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);
    assert_eq!(back, PutBack::default());
}

#[test]
fn a_file_the_project_context_already_carries_takes_no_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    for (name, age) in [
        ("CLAUDE.md", 1),
        ("a", 10),
        ("b", 20),
        ("c", 30),
        ("d", 40),
        ("e", 50),
    ] {
        write_aged(&root, name, format!("contents of {name}"), age);
    }
    std::os::unix::fs::symlink(root.join("CLAUDE.md"), root.join("AGENTS.md")).unwrap();
    let touched = touched_by(
        &["CLAUDE.md", "a", "b", "c", "d", "e"].map(|n| ("read_file", n, false)),
        &root,
    );
    // Named through a link, as the instruction files may be.
    let carried = [root.join("AGENTS.md")];
    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &carried);
    assert_eq!(back.read, ["a", "b", "c", "d", "e"].map(|n| root.join(n)));
    let message = back.message.unwrap();
    assert!(!message_text(&message).contains("contents of CLAUDE.md"));
}

fn thinking() -> ContentBlock {
    ContentBlock(json!({ "type": "thinking", "thinking": "reasoning", "signature": "c2ln" }))
}

fn assistant_calling(id: &str) -> MessageParam {
    MessageParam {
        role: Role::Assistant,
        content: vec![
            thinking(),
            ContentBlock(json!({ "type": "tool_use", "id": id, "name": "shell", "input": {} })),
        ],
    }
}

fn long_result(id: &str) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::tool_result(id, "o".repeat(2_000), false)],
    }
}

fn assistant(text: &str) -> MessageParam {
    MessageParam {
        role: Role::Assistant,
        content: vec![thinking(), ContentBlock::text(text)],
    }
}

fn all_text(messages: &[MessageParam]) -> String {
    serde_json::to_string(messages).unwrap()
}

#[test]
fn a_shrunk_summary_request_leaves_out_what_is_read_again_and_the_bulk() {
    let history = vec![
        user("<project-context>\nBIG INSTRUCTIONS\n</project-context>"),
        user("first request"),
        assistant_calling("a"),
        long_result("a"),
        assistant("first answer"),
        user("<working-folder>\nMOVED\n</working-folder>"),
        user("<project-context-update>\nBIG UPDATE\n</project-context-update>"),
        user("<recent-files>\nBIG FILES\n</recent-files>"),
        user("second request"),
        assistant("second answer"),
    ];
    let request = shrunk_summary_request(&history, 0).unwrap();
    let text = all_text(&request);
    for gone in [
        "BIG INSTRUCTIONS",
        "MOVED",
        "BIG UPDATE",
        "BIG FILES",
        "ooooo",
        "reasoning",
    ] {
        assert!(!text.contains(gone), "{gone} should be gone");
    }
    for kept in [
        "first request",
        "first answer",
        "second request",
        "second answer",
    ] {
        assert!(text.contains(kept), "{kept} should be kept");
    }
    assert_eq!(request.last(), Some(&user(SUMMARY_REQUEST)));
    assert!(text.contains("\"tool_use\"") && text.contains("\"tool_result\""));
}

#[test]
fn a_user_message_merely_mentioning_a_carried_tag_is_kept() {
    let history = vec![
        user("what does <project-context> mean?"),
        assistant("a tag"),
    ];
    let request = shrunk_summary_request(&history, 0).unwrap();
    assert!(all_text(&request).contains("what does"));
}

#[test]
fn only_messages_read_again_leave_nothing_to_summarize() {
    let history = vec![user("<project-context>\nBIG\n</project-context>")];
    assert_eq!(shrunk_summary_request(&history, 0), None);
    assert_eq!(shrunk_summary_request(&[], 0), None);
}

#[test]
fn a_reply_left_with_only_thinking_keeps_a_block() {
    let history = vec![
        user("hi"),
        MessageParam {
            role: Role::Assistant,
            content: vec![thinking()],
        },
    ];
    let request = shrunk_summary_request(&history, 0).unwrap();
    assert!(!request[1].content.is_empty());
    assert_eq!(request[1].content[0].block_type(), "text");
}

fn exchanges(n: usize) -> Vec<MessageParam> {
    (0..n)
        .flat_map(|i| {
            let id = format!("call_{i}");
            [
                user(&format!("request {i}")),
                assistant_calling(&id),
                long_result(&id),
                assistant(&format!("answer {i}")),
            ]
        })
        .collect()
}

#[test]
fn each_further_attempt_keeps_about_half_of_what_was_left_the_newest() {
    let history = exchanges(8);
    let kept = |attempt: u32| {
        let request = shrunk_summary_request(&history, attempt).unwrap();
        let text = all_text(&request);
        (0..8)
            .filter(|i| text.contains(&format!("request {i}\"")))
            .collect::<Vec<_>>()
    };
    assert_eq!(kept(0), (0..8).collect::<Vec<_>>());
    assert_eq!(kept(1), (4..8).collect::<Vec<_>>());
    assert_eq!(kept(2), (6..8).collect::<Vec<_>>());
    assert_eq!(kept(3), [7]);

    let first = &shrunk_summary_request(&history, 1).unwrap()[0];
    assert_eq!(first, &user(LEFT_OUT));
    assert!(!all_text(&shrunk_summary_request(&history, 0).unwrap()).contains(LEFT_OUT));
}

#[test]
fn nothing_further_can_be_left_out_of_one_exchange() {
    let history = exchanges(1);
    assert!(shrunk_summary_request(&history, 0).is_some());
    assert_eq!(shrunk_summary_request(&history, 1), None);
}

#[test]
fn a_cut_never_parts_a_tool_call_from_its_result() {
    for n in 1..=7 {
        let history = exchanges(n);
        for attempt in 0..5 {
            let Some(request) = shrunk_summary_request(&history, attempt) else {
                continue;
            };
            let mut open = Vec::new();
            for message in &request {
                for block in &message.content {
                    if block.block_type() == "tool_use" {
                        open.push(block.0["id"].as_str().unwrap().to_string());
                    }
                    if block.block_type() == "tool_result" {
                        let id = block.0["tool_use_id"].as_str().unwrap();
                        let at = open.iter().position(|o| o == id);
                        assert!(at.is_some(), "a result with no call, {n} {attempt}");
                        open.remove(at.unwrap());
                    }
                }
            }
            assert!(open.is_empty(), "a call with no result, {n} {attempt}");
            assert_eq!(request[0].role, Role::User);
        }
    }
}

#[test]
fn a_tail_of_tool_rounds_with_no_later_request_cannot_be_cut() {
    // One request followed by many rounds has one boundary, at its start.
    let mut history = vec![user("the only request")];
    for i in 0..6 {
        let id = format!("c{i}");
        history.push(assistant_calling(&id));
        history.push(long_result(&id));
    }
    assert_eq!(shrunk_summary_request(&history, 1), None);
}

#[test]
fn a_users_line_opening_with_a_carried_tag_is_kept() {
    let history = vec![
        user("<working-folder> what does this tag mean?"),
        assistant("a tag"),
    ];
    let request = shrunk_summary_request(&history, 0).unwrap();
    assert!(all_text(&request).contains("what does this tag mean"));
    assert_eq!(request[0].role, Role::User);
}

#[test]
fn the_last_resort_asks_for_a_summary_of_nothing_kept() {
    let request = last_resort_summary_request();
    assert_eq!(request, vec![user(LEFT_OUT), user(SUMMARY_REQUEST)]);
}

#[test]
fn an_image_does_not_come_back_or_take_a_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut large = png.clone();
    large.resize(20_001, 0);
    write_aged(&root, "small.png", &png, 1);
    write_aged(&root, "large.png", &large, 2);
    write_aged(&root, "no_extension", &large, 3);
    let mut calls = vec![("read_file", "small.png", false)];
    calls.push(("read_file", "large.png", false));
    calls.push(("read_file", "no_extension", false));
    let names = ["a", "b", "c", "d", "e"];
    for (age, name) in names.iter().enumerate() {
        write_aged(&root, name, "text\n", 10 + age as u64);
        calls.push(("read_file", name, false));
    }
    let touched = touched_by(&calls, &root);

    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);

    let expected: Vec<PathBuf> = names.iter().map(|n| root.join(n)).collect();
    assert_eq!(back.read, expected);
    assert!(back.referenced.is_empty(), "{:?}", back.referenced);
    let text = message_text(back.message.as_ref().unwrap());
    assert!(
        !text.contains(".png") && !text.contains("no_extension"),
        "{text}"
    );
}

const PICTURE: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAIAAACQd1PeAAAADElEQVR4nGP4z8AAAAMBAQDJ/pLvAAAAAElFTkSuQmCC";

fn notebook_with_picture() -> String {
    json!({
        "cells": [{ "cell_type": "code", "id": "c1", "metadata": {}, "source": ["plot()"],
                    "outputs": [{ "output_type": "display_data", "metadata": {},
                                  "data": { "image/png": PICTURE, "text/plain": "<Figure>" } }] }],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
    })
    .to_string()
}

#[test]
fn a_notebook_comes_back_as_its_cells_with_its_images_after_the_text() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "n.ipynb", notebook_with_picture(), 1);
    write_aged(&root, "a.txt", "alpha\n", 2);
    let touched = touched_by(
        &[
            ("read_file", "n.ipynb", false),
            ("read_file", "a.txt", false),
        ],
        &root,
    );

    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);

    assert_eq!(back.read, [root.join("n.ipynb"), root.join("a.txt")]);
    let message = back.message.unwrap();
    assert_eq!(message.content.len(), 2);
    let text = message_text(&message);
    assert!(text.starts_with("<recent-files>\n"), "{text}");
    assert!(text.ends_with("</recent-files>"), "{text}");
    assert!(
        text.contains(
            "Contents of n.ipynb:\n\n<cell id=\"c1\">plot()</cell id=\"c1\">\n\n<Figure>"
        ),
        "{text}"
    );
    assert!(
        !text.contains("nbformat") && !text.contains(PICTURE),
        "{text}"
    );
    assert_eq!(message.content[1].0["type"], "image");
}

#[test]
fn a_notebook_over_the_size_comes_back_as_its_path_and_a_broken_one_not_at_all() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "big.ipynb", "x".repeat(20_001), 1);
    write_aged(&root, "broken.ipynb", "{not json", 2);
    let touched = touched_by(
        &[
            ("read_file", "big.ipynb", false),
            ("read_file", "broken.ipynb", false),
        ],
        &root,
    );

    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);

    assert_eq!(back.referenced, [root.join("big.ipynb")]);
    assert!(back.read.is_empty(), "{:?}", back.read);
}

#[test]
fn recent_files_carrying_an_image_are_left_out_of_a_shrunk_summary_request() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "n.ipynb", notebook_with_picture(), 1);
    let touched = touched_by(&[("read_file", "n.ipynb", false)], &root);
    let carried = put_back(&touched, &ToolContext::new(&root).unwrap(), &[])
        .message
        .unwrap();
    let line = MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text("hello")],
    };

    let request = shrunk_summary_request(&[carried, line.clone()], 0).unwrap();

    assert_eq!(request[0], line);
    assert_eq!(request.len(), 2, "the line and the summary request");
}

#[test]
fn a_pdf_does_not_come_back_or_take_a_place() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "a.pdf", "%PDF-1.4\n", 1);
    write_aged(&root, "big.pdf", "x".repeat(20_001), 2);
    let touched = touched_by(
        &[
            ("read_file", "a.pdf", false),
            ("read_file", "big.pdf", false),
        ],
        &root,
    );

    let back = put_back(&touched, &ToolContext::new(&root).unwrap(), &[]);

    assert_eq!(back, PutBack::default());
}

#[test]
fn a_notebook_image_the_api_refused_does_not_come_back() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    write_aged(&root, "n.ipynb", notebook_with_picture(), 1);
    let mut touched = touched_by(&[("read_file", "n.ipynb", false)], &root);
    let ctx = ToolContext::new(&root).unwrap();
    let image = put_back(&touched, &ctx, &[]).message.unwrap().content[1].clone();

    touched.refuse(&image.0);
    let back = put_back(&touched, &ctx, &[]);

    let message = back.message.unwrap();
    assert_eq!(message.content.len(), 1, "{:?}", message.content);
    assert!(message_text(&message).contains("<cell id=\"c1\">"));
}
