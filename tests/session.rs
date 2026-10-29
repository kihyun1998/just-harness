mod support;

use std::path::Path;

use just_harness::api::{ContentBlock, MessageParam, Role};
use just_harness::clearing::{CLEARED, clear_before};
use just_harness::compaction::compacted;
use just_harness::session::{
    FORMAT_VERSION, Prompt, SessionLog, SessionMeta, latest, load, new_id, sessions_dir,
};
use serde_json::json;

fn meta(id: &str) -> SessionMeta {
    SessionMeta {
        version: FORMAT_VERSION,
        id: id.into(),
        created: 1_761_700_000,
        cwd: "/work/project".into(),
        model: "claude-haiku-5-5".into(),
    }
}

fn text(role: Role, text: &str) -> MessageParam {
    MessageParam {
        role,
        content: vec![ContentBlock::text(text)],
    }
}

fn tool_call(id: &str) -> MessageParam {
    MessageParam {
        role: Role::Assistant,
        content: vec![
            ContentBlock(json!({ "type": "thinking", "thinking": "", "signature": "c2ln" })),
            ContentBlock(
                json!({ "type": "tool_use", "id": id, "name": "shell", "input": { "command": "ls" } }),
            ),
        ],
    }
}

/// Longer than the placeholder a clear puts in its place.
const LISTING: &str = "a.txt\nb.txt\nc.txt\nd.txt\ne.txt\nf.txt\ng.txt\nh.txt\ni.txt\nj.txt\nk.txt\nl.txt\nm.txt\nn.txt\n";

fn tool_result(id: &str) -> MessageParam {
    MessageParam {
        role: Role::User,
        content: vec![ContentBlock::tool_result(id, LISTING, false)],
    }
}

#[test]
fn a_session_reads_back_as_it_was_written() {
    let dir = tempfile::tempdir().unwrap();
    let messages = vec![
        text(Role::User, "list files"),
        tool_call("toolu_1"),
        tool_result("toolu_1"),
        text(Role::Assistant, "One file."),
    ];
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    for m in &messages {
        log.append(m).unwrap();
    }

    let loaded = load(log.path()).unwrap();

    assert_eq!(log.path(), dir.path().join("s1.jsonl"));
    assert_eq!(loaded.meta, meta("s1"));
    assert_eq!(loaded.history, messages);
    assert_eq!(loaded.skipped, 0);
    assert_eq!(loaded.repair, None);
}

#[test]
fn each_append_is_on_disk_before_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();

    let lines = std::fs::read_to_string(log.path()).unwrap().lines().count();

    assert_eq!(lines, 2);
}

#[test]
fn a_dropped_turn_stays_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();
    log.append(&text(Role::Assistant, "answer")).unwrap();
    log.append(&text(Role::User, "second")).unwrap();
    log.append(&tool_call("toolu_1")).unwrap();
    log.drop_to(2).unwrap();
    log.append(&text(Role::User, "third")).unwrap();

    let loaded = load(log.path()).unwrap();

    assert_eq!(
        loaded.history,
        vec![
            text(Role::User, "first"),
            text(Role::Assistant, "answer"),
            text(Role::User, "third")
        ]
    );
}

#[test]
fn a_clear_is_applied_again_on_load_where_it_was_recorded() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    let mut history = vec![
        text(Role::User, "list files"),
        tool_call("toolu_1"),
        tool_result("toolu_1"),
        tool_call("toolu_2"),
        tool_result("toolu_2"),
        text(Role::Assistant, "Two files."),
    ];
    for m in &history {
        log.append(m).unwrap();
    }
    clear_before(&mut history, 4);
    log.record_clear(4).unwrap();
    let later = vec![text(Role::User, "again"), tool_call("toolu_3")];
    for m in &later {
        log.append(m).unwrap();
        history.push(m.clone());
    }

    let loaded = load(log.path()).unwrap();

    assert_eq!(loaded.history[2].content[0].0["content"], CLEARED);
    assert_eq!(loaded.history[4].content[0].0["content"], LISTING);
    // The thinking made after the clear is kept.
    assert_eq!(loaded.history[7].content[0].block_type(), "thinking");
    assert_eq!(loaded.history[..8], history[..]);
    assert_eq!(loaded.skipped, 0);
}

#[test]
fn a_compaction_replaces_the_history_with_its_summary_on_load() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    for m in [
        text(Role::User, "list files"),
        tool_call("toolu_1"),
        tool_result("toolu_1"),
        text(Role::Assistant, "One file."),
    ] {
        log.append(&m).unwrap();
    }
    log.record_compact("Listed the files: one.").unwrap();
    let mut expected = compacted("Listed the files: one.");
    for m in [
        text(Role::User, "again"),
        text(Role::Assistant, "Still one."),
    ] {
        log.append(&m).unwrap();
        expected.push(m);
    }

    let loaded = load(log.path()).unwrap();

    assert_eq!(loaded.history, expected);
    assert_eq!(loaded.skipped, 0);
}

#[test]
fn a_drop_after_a_compaction_counts_from_the_summary() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();
    log.append(&text(Role::Assistant, "answer")).unwrap();
    log.record_compact("Said hello.").unwrap();
    log.append(&text(Role::User, "second")).unwrap();
    log.append(&tool_call("toolu_1")).unwrap();
    log.drop_to(1).unwrap();

    let loaded = load(log.path()).unwrap();

    assert_eq!(loaded.history, compacted("Said hello."));
}

#[test]
fn a_turn_dropped_after_a_clear_leaves_the_cleared_history() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    let mut history = vec![
        text(Role::User, "list files"),
        tool_call("toolu_1"),
        tool_result("toolu_1"),
        text(Role::Assistant, "One file."),
    ];
    for m in &history {
        log.append(m).unwrap();
    }
    clear_before(&mut history, 3);
    log.record_clear(3).unwrap();
    log.append(&text(Role::User, "again")).unwrap();
    log.append(&tool_call("toolu_2")).unwrap();
    log.drop_to(4).unwrap();

    let loaded = load(log.path()).unwrap();

    assert_eq!(loaded.history, history);
}

#[test]
fn a_second_clear_keeps_the_thinking_made_after_the_first_up_to_its_own_change() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    let mut history = vec![
        text(Role::User, "list files"),
        tool_call("toolu_1"),
        tool_result("toolu_1"),
        tool_call("toolu_2"),
        tool_result("toolu_2"),
        text(Role::Assistant, "Two files."),
    ];
    for m in &history {
        log.append(m).unwrap();
    }
    clear_before(&mut history, 6);
    log.record_clear(6).unwrap();
    for m in [
        text(Role::User, "again"),
        tool_call("toolu_3"),
        tool_result("toolu_3"),
        text(Role::Assistant, "Three."),
    ] {
        log.append(&m).unwrap();
        history.push(m);
    }
    clear_before(&mut history, 10);
    log.record_clear(10).unwrap();

    let loaded = load(log.path()).unwrap();

    assert_eq!(loaded.history, history);
    assert_eq!(loaded.history[8].content[0].0["content"], CLEARED);
    // Made after the first clear, before the second one's change.
    assert_eq!(loaded.history[7].content[0].block_type(), "thinking");
    assert_eq!(loaded.history[3].content[0].block_type(), "tool_use");
}

#[test]
fn an_unanswered_tool_call_at_the_end_is_answered_as_not_run() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "list files")).unwrap();
    log.append(&tool_call("toolu_9")).unwrap();

    let loaded = load(log.path()).unwrap();

    let repair = MessageParam {
        role: Role::User,
        content: vec![ContentBlock(json!({
            "type": "tool_result", "tool_use_id": "toolu_9",
            "content": "No result: the session ended before this call's result was recorded, so it may or may not have run.", "is_error": true
        }))],
    };
    assert_eq!(loaded.repair, Some(repair.clone()));
    assert_eq!(loaded.history.len(), 3);
    assert_eq!(loaded.history[2], repair);
}

#[test]
fn a_cut_last_line_is_skipped_and_counted() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();
    log.append(&text(Role::Assistant, "answer")).unwrap();
    let path = log.path().to_path_buf();
    drop(log);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"type\":\"message\",\"message\":{\"role\":\"us");
    std::fs::write(&path, bytes).unwrap();

    let loaded = load(&path).unwrap();

    assert_eq!(loaded.skipped, 1);
    assert_eq!(
        loaded.history,
        vec![text(Role::User, "first"), text(Role::Assistant, "answer")]
    );
}

#[test]
fn a_reopened_session_appends_after_what_is_there() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();
    let path = log.path().to_path_buf();
    drop(log);

    let mut again = SessionLog::open(&path).unwrap();
    again.append(&text(Role::Assistant, "answer")).unwrap();

    assert_eq!(
        load(&path).unwrap().history,
        vec![text(Role::User, "first"), text(Role::Assistant, "answer")]
    );
}

#[test]
fn a_file_that_does_not_start_with_session_meta_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.jsonl");
    std::fs::write(
        &path,
        "{\"type\":\"message\",\"message\":{\"role\":\"user\",\"content\":[]}}\n",
    )
    .unwrap();

    let err = load(&path).unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn create_refuses_to_overwrite_a_session() {
    let dir = tempfile::tempdir().unwrap();
    SessionLog::create(dir.path(), &meta("s1")).unwrap();

    assert!(SessionLog::create(dir.path(), &meta("s1")).is_err());
}

#[test]
fn sessions_are_kept_per_working_folder_under_home() {
    assert_eq!(
        sessions_dir(
            Path::new("/home/me"),
            Path::new("/Volumes/T7/GitHub/just-harness")
        ),
        Path::new("/home/me/.just-harness/sessions/-Volumes-T7-GitHub-just-harness")
    );
}

#[test]
fn latest_is_the_most_recently_written_session_of_this_folder() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(latest(dir.path(), "/work/project").unwrap(), None);
    let base = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
    let mut other = meta("other");
    other.cwd = "/work-project".into();
    for (m, age_secs) in [
        (meta("a"), 300),
        (meta("b"), 600),
        (meta("c"), 0),
        (other, 0),
    ] {
        let mut log = SessionLog::create(dir.path(), &m).unwrap();
        log.append(&text(Role::User, "hi")).unwrap();
        let file = std::fs::File::options()
            .write(true)
            .open(log.path())
            .unwrap();
        file.set_modified(base - std::time::Duration::from_secs(age_secs))
            .unwrap();
    }
    std::fs::write(dir.path().join("notes.txt"), "not a session").unwrap();
    std::fs::write(dir.path().join("broken.jsonl"), "not json\n").unwrap();

    assert_eq!(
        latest(dir.path(), "/work/project").unwrap(),
        Some(dir.path().join("c.jsonl"))
    );
    assert_eq!(
        latest(dir.path(), "/work-project").unwrap(),
        Some(dir.path().join("other.jsonl"))
    );
}

#[test]
fn a_session_written_by_a_newer_format_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let mut newer = meta("s1");
    newer.version = FORMAT_VERSION + 1;
    let log = SessionLog::create(dir.path(), &newer).unwrap();

    let err = load(log.path()).unwrap_err();

    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    assert!(err.to_string().contains("newer"), "{err}");
}

#[test]
fn a_tool_call_left_unanswered_inside_the_history_cuts_it_there() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();
    log.append(&text(Role::Assistant, "answer")).unwrap();
    log.append(&text(Role::User, "list files")).unwrap();
    log.append(&tool_call("toolu_1")).unwrap();
    let path = log.path().to_path_buf();
    drop(log);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{not json\n");
    std::fs::write(&path, bytes).unwrap();
    let mut log = SessionLog::open(&path).unwrap();
    log.append(&text(Role::Assistant, "One file.")).unwrap();

    let loaded = load(&path).unwrap();

    assert_eq!(loaded.skipped, 1);
    assert_eq!(loaded.cut_at, Some(3));
    assert_eq!(
        loaded.history,
        vec![
            text(Role::User, "first"),
            text(Role::Assistant, "answer"),
            text(Role::User, "list files")
        ]
    );
    assert_eq!(loaded.repair, None);
}

#[cfg(unix)]
#[test]
fn session_files_and_folders_are_private_to_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join("sessions").join("x");
    let log = SessionLog::create(&dir, &meta("s1")).unwrap();

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;

    assert_eq!(mode(log.path()), 0o600);
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&home.path().join("sessions")), 0o700);
}

#[test]
fn new_ids_differ() {
    let (a, b) = (new_id(), new_id());
    assert!(!a.is_empty());
    assert_ne!(a, b);
}

#[tokio::test]
async fn a_resumed_session_with_a_dangling_call_sends_a_valid_next_request() {
    use just_harness::agent::{Agent, AllowAll};
    use just_harness::api::Client;
    use just_harness::tools::{ToolContext, builtin};
    use support::{FakeTransport, config, sent_body, text_reply};

    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "list files")).unwrap();
    log.append(&tool_call("toolu_9")).unwrap();
    let mut history = load(log.path()).unwrap().history;
    history.push(text(Role::User, "never mind, just say hi"));
    let transport = FakeTransport::new().respond(200, vec![&text_reply("hi", "end_turn")]);
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        ToolContext::new(dir.path()).unwrap(),
    );

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    let messages = &sent_body(&transport, 0)["messages"];
    assert_eq!(messages.as_array().unwrap().len(), 4);
    assert_eq!(messages[1]["content"][1]["id"], "toolu_9");
    assert_eq!(messages[2]["content"][0]["tool_use_id"], "toolu_9");
    assert_eq!(messages[2]["content"][0]["is_error"], true);
    assert_eq!(messages[3]["content"][0]["text"], "never mind, just say hi");
}

#[test]
fn appending_after_a_cut_last_line_starts_a_new_line() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "first")).unwrap();
    let path = log.path().to_path_buf();
    drop(log);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"{\"type\":\"message\",\"message\":{\"role\":\"us");
    std::fs::write(&path, bytes).unwrap();

    let mut again = SessionLog::open(&path).unwrap();
    again.append(&text(Role::User, "after the crash")).unwrap();
    let loaded = load(&path).unwrap();

    assert_eq!(loaded.skipped, 1);
    assert_eq!(
        loaded.history,
        vec![
            text(Role::User, "first"),
            text(Role::User, "after the crash")
        ]
    );
}

fn prompt(text: &str) -> Prompt {
    Prompt {
        system: vec![ContentBlock::text(text)],
        tools: vec![just_harness::api::Tool {
            name: "read_file".into(),
            description: format!("Reads a file. {text}"),
            input_schema: json!({ "type": "object" }),
            cache_control: None,
        }],
    }
}

#[test]
fn the_last_recorded_prompt_comes_back_and_a_compaction_keeps_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.record_prompt(&prompt("first")).unwrap();
    log.append(&text(Role::User, "hi")).unwrap();
    log.append(&text(Role::Assistant, "hello")).unwrap();
    assert_eq!(load(log.path()).unwrap().prompt, Some(prompt("first")));

    log.record_compact("Said hello.").unwrap();
    assert_eq!(load(log.path()).unwrap().prompt, Some(prompt("first")));
    log.record_prompt(&prompt("second")).unwrap();
    log.append(&text(Role::User, "again")).unwrap();

    let loaded = load(log.path()).unwrap();
    assert_eq!(loaded.prompt, Some(prompt("second")));
    assert_eq!(loaded.history.len(), 2);
    assert_eq!(loaded.skipped, 0);
}

#[test]
fn a_session_with_no_recorded_prompt_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = SessionLog::create(dir.path(), &meta("s1")).unwrap();
    log.append(&text(Role::User, "hi")).unwrap();
    assert_eq!(load(log.path()).unwrap().prompt, None);
}
