use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use just_harness::api::{ContentBlock, Message, MessageParam, Role, StopReason, Usage};
use just_harness::compaction::{
    CompactGate, CompactGuard, CompactPolicy, PutBack, SUMMARY_PREFIX, SUMMARY_REQUEST,
    TouchedFiles, compacted, put_back, should_compact, summary, summary_request,
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
