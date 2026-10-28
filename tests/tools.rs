//! The built-in tools, driven through the agent loop: a scripted reply calls
//! the tool, and the result is read from the next request.

mod support;

use std::path::Path;

use just_harness::agent::{Agent, AllowAll};
use just_harness::api::Client;
use just_harness::tools::{ToolContext, builtin};
use serde_json::{Value, json};
use support::{FakeTransport, config, sent_body, text_reply, tool_reply, user};

/// Runs `name` with `input` in `root` and returns the result's content and
/// whether it was an error.
async fn call(root: &Path, name: &str, input: Value) -> (String, bool) {
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", name, input)], "tool_use")],
        )
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let context = ToolContext::new(root).unwrap();
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        context,
    );
    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();
    let result = &sent_body(&transport, 1)["messages"][2]["content"][0];
    (
        result["content"].as_str().unwrap().to_string(),
        result["is_error"].as_bool().unwrap_or(false),
    )
}

fn write(root: &Path, path: &str, content: &str) {
    let path = root.join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn read_file_numbers_lines_and_honours_offset_and_limit() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.txt", "one\ntwo\nthree\nfour\n");

    let (whole, err) = call(dir.path(), "read_file", json!({ "path": "src/a.txt" })).await;
    assert!(!err, "{whole}");
    assert_eq!(
        whole,
        "     1\tone\n     2\ttwo\n     3\tthree\n     4\tfour"
    );

    let (part, _) = call(
        dir.path(),
        "read_file",
        json!({ "path": "src/a.txt", "offset": 2, "limit": 2 }),
    )
    .await;
    assert_eq!(
        part,
        "     2\ttwo\n     3\tthree\n[lines 2-3 of 4; more from offset 4]"
    );
}

#[tokio::test]
async fn file_tools_refuse_paths_outside_the_working_folder() {
    let outer = tempfile::tempdir().unwrap();
    write(outer.path(), "secret.txt", "secret");
    write(outer.path(), "work/inside.txt", "inside");
    let root = outer.path().join("work");
    std::os::unix::fs::symlink(outer.path().join("secret.txt"), root.join("link.txt")).unwrap();
    let absolute_inside = root.canonicalize().unwrap().join("inside.txt");

    for path in [
        "../secret.txt",
        outer.path().join("secret.txt").to_str().unwrap(),
        "link.txt",
        "sub/../../secret.txt",
    ] {
        let (out, err) = call(&root, "read_file", json!({ "path": path })).await;
        assert!(
            err && out.contains("outside the working folder"),
            "{path}: {out}"
        );
        assert!(
            !out.contains("secret\n") && out != "     1\tsecret",
            "{path}: {out}"
        );
    }
    let (out, err) = call(
        &root,
        "read_file",
        json!({ "path": absolute_inside.to_str().unwrap() }),
    )
    .await;
    assert!(!err, "{out}");
    assert_eq!(out, "     1\tinside");
}

#[tokio::test]
async fn read_file_reports_a_missing_file_as_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "nope.txt" })).await;
    assert!(err);
    assert!(out.starts_with("nope.txt: "), "{out}");
}

#[tokio::test]
async fn edit_file_replaces_exactly_one_match() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.rs", "fn a() {}\nfn b() {}\n");

    let (out, err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": "a.rs", "old_string": "fn b() {}", "new_string": "fn b() { a() }" }),
    )
    .await;

    assert!(!err, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        "fn a() {}\nfn b() { a() }\n"
    );
}

#[tokio::test]
async fn edit_file_refuses_zero_or_several_matches_and_leaves_the_file_alone() {
    let dir = tempfile::tempdir().unwrap();
    let original = "let x = 1;\nlet y = 2;\nlet x = 1;\n";
    write(dir.path(), "a.rs", original);

    let (none, none_err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": "a.rs", "old_string": "let z", "new_string": "q" }),
    )
    .await;
    let (many, many_err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": "a.rs", "old_string": "let x = 1;", "new_string": "q" }),
    )
    .await;

    assert!(none_err && none.contains("not found"), "{none}");
    assert!(
        many_err
            && many.contains("2 matches")
            && many.contains("line 1")
            && many.contains("line 3"),
        "{many}"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.rs")).unwrap(),
        original
    );
}

#[tokio::test]
async fn edit_file_refuses_a_path_outside_the_working_folder() {
    let outer = tempfile::tempdir().unwrap();
    write(outer.path(), "other.rs", "keep");
    std::fs::create_dir(outer.path().join("work")).unwrap();

    let (out, err) = call(
        &outer.path().join("work"),
        "edit_file",
        json!({ "path": "../other.rs", "old_string": "keep", "new_string": "gone" }),
    )
    .await;

    assert!(err && out.contains("outside the working folder"), "{out}");
    assert_eq!(
        std::fs::read_to_string(outer.path().join("other.rs")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn edit_file_with_an_empty_old_string_creates_a_new_file_only() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "exists.txt", "keep");

    let (created, created_err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": "new/b.txt", "old_string": "", "new_string": "hello\n" }),
    )
    .await;
    let (exists, exists_err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": "exists.txt", "old_string": "", "new_string": "x" }),
    )
    .await;

    assert!(!created_err, "{created}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("new/b.txt")).unwrap(),
        "hello\n"
    );
    assert!(exists_err && exists.contains("exists"), "{exists}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("exists.txt")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn shell_reports_exit_code_stdout_and_stderr_from_the_working_folder() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "marker.txt", "");

    let (out, err) = call(
        dir.path(),
        "shell",
        json!({ "command": "ls; echo oops >&2; exit 3" }),
    )
    .await;

    assert!(
        !err,
        "a non-zero exit is a result, not a tool failure: {out}"
    );
    assert_eq!(
        out,
        "exit code: 3\n--- stdout ---\nmarker.txt\n--- stderr ---\noops"
    );
}

#[tokio::test]
async fn shell_kills_the_whole_command_at_its_timeout() {
    let dir = tempfile::tempdir().unwrap();
    let marker = "sleep 37.25";
    let alive = || {
        std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(!alive(), "a `{marker}` process is already running");

    let (out, err) = call(
        dir.path(),
        "shell",
        json!({ "command": format!("{marker} | cat"), "timeout_secs": 1 }),
    )
    .await;

    assert!(err && out.contains("timed out after 1 s"), "{out}");
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!alive(), "`{marker}` outlived the timeout");
}

#[tokio::test]
async fn shell_keeps_the_end_of_long_output_and_says_so() {
    let dir = tempfile::tempdir().unwrap();

    let (out, _) = call(dir.path(), "shell", json!({ "command": "seq 1 5000" })).await;

    let stdout = out.split("--- stdout ---\n").nth(1).unwrap();
    assert!(
        stdout.starts_with(
            "[output truncated: 5000 lines, 23893 bytes; showing the last 2000 lines]\n3001\n3002\n"
        ),
        "{stdout:.120}"
    );
    assert!(stdout.ends_with("\n4999\n5000"));
    assert!(!stdout.contains("\n3000\n"));
}

#[tokio::test]
async fn search_finds_lines_by_regex_and_skips_ignored_and_hidden_files() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".gitignore", "target/\n");
    write(dir.path(), "src/a.rs", "fn main() {}\nfn helper() {}\n");
    write(dir.path(), "src/b.rs", "// fn nothing\n");
    write(dir.path(), "target/gen.rs", "fn generated() {}\n");
    write(dir.path(), ".env", "fn secret() {}\n");

    let (out, err) = call(dir.path(), "search", json!({ "pattern": "^fn \\w+" })).await;

    assert!(!err, "{out}");
    assert_eq!(out, "src/a.rs:1:fn main() {}\nsrc/a.rs:2:fn helper() {}");
}

#[tokio::test]
async fn search_lists_files_by_glob_under_a_path() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "src/a.rs", "");
    write(dir.path(), "src/deep/b.rs", "");
    write(dir.path(), "src/c.txt", "");
    write(dir.path(), "docs/d.rs", "");

    let (out, err) = call(
        dir.path(),
        "search",
        json!({ "glob": "**/*.rs", "path": "src" }),
    )
    .await;

    assert!(!err, "{out}");
    assert_eq!(out, "src/a.rs\nsrc/deep/b.rs");
}

#[tokio::test]
async fn search_combines_pattern_and_glob_and_reports_no_matches() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.rs", "todo: rust\n");
    write(dir.path(), "a.md", "todo: docs\n");

    let (both, _) = call(
        dir.path(),
        "search",
        json!({ "pattern": "todo", "glob": "*.md" }),
    )
    .await;
    let (none, none_err) = call(dir.path(), "search", json!({ "pattern": "absent" })).await;

    assert_eq!(both, "a.md:1:todo: docs");
    assert!(!none_err);
    assert_eq!(
        none,
        "no matches (hidden files and paths in .gitignore are not searched)"
    );
}

#[tokio::test]
async fn search_rejects_a_bad_regex_and_a_path_outside_the_working_folder() {
    let outer = tempfile::tempdir().unwrap();
    std::fs::create_dir(outer.path().join("work")).unwrap();
    let root = outer.path().join("work");

    let (bad, bad_err) = call(&root, "search", json!({ "pattern": "(" })).await;
    let (outside, outside_err) =
        call(&root, "search", json!({ "pattern": "x", "path": ".." })).await;
    let (neither, neither_err) = call(&root, "search", json!({})).await;

    assert!(bad_err && bad.contains("regex"), "{bad}");
    assert!(
        outside_err && outside.contains("outside the working folder"),
        "{outside}"
    );
    assert!(
        neither_err && neither.contains("`pattern` or `glob`"),
        "{neither}"
    );
}

#[tokio::test]
async fn a_glob_without_a_slash_matches_names_and_one_with_a_slash_matches_paths() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "top.md", "");
    write(dir.path(), "docs/nested.md", "");
    write(dir.path(), "src/a.rs", "");
    write(dir.path(), "src/deep/b.rs", "");

    let (by_name, _) = call(dir.path(), "search", json!({ "glob": "*.md" })).await;
    let (by_path, _) = call(dir.path(), "search", json!({ "glob": "src/*.rs" })).await;

    assert_eq!(by_name, "docs/nested.md\ntop.md");
    assert_eq!(by_path, "src/a.rs");
}

#[tokio::test]
async fn a_dangling_symlink_cannot_lead_a_new_file_outside() {
    let outer = tempfile::tempdir().unwrap();
    std::fs::create_dir(outer.path().join("work")).unwrap();
    let root = outer.path().join("work");
    std::os::unix::fs::symlink(outer.path().join("planted.txt"), root.join("link.txt")).unwrap();

    let (out, err) = call(
        &root,
        "edit_file",
        json!({ "path": "link.txt", "old_string": "", "new_string": "x" }),
    )
    .await;

    assert!(err, "{out}");
    assert!(
        !outer.path().join("planted.txt").exists(),
        "a file was written outside the working folder"
    );
}

#[tokio::test]
async fn shell_shows_the_end_of_a_line_longer_than_the_byte_limit() {
    let dir = tempfile::tempdir().unwrap();

    let (out, _) = call(
        dir.path(),
        "shell",
        json!({ "command": "head -c 200000 /dev/zero | tr '\\0' a; printf b" }),
    )
    .await;

    let stdout = out.split("--- stdout ---\n").nth(1).unwrap();
    assert_eq!(
        stdout,
        format!(
            "[output truncated: 1 lines, 200001 bytes; showing the last 1 lines]\n{}b",
            "a".repeat(49_999)
        )
    );
}

#[tokio::test]
async fn shell_returns_when_the_command_exits_even_if_a_background_job_holds_its_output() {
    let dir = tempfile::tempdir().unwrap();
    let started = std::time::Instant::now();

    let (out, err) = call(
        dir.path(),
        "shell",
        json!({ "command": "sleep 3.75 & echo started", "timeout_secs": 10 }),
    )
    .await;

    assert!(!err, "{out}");
    assert_eq!(out, "exit code: 0\n--- stdout ---\nstarted");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "took {:?}",
        started.elapsed()
    );
    let _ = std::process::Command::new("pkill")
        .args(["-f", "sleep 3.75"])
        .status();
}

#[tokio::test]
async fn shell_keeps_what_a_command_printed_before_its_timeout() {
    let dir = tempfile::tempdir().unwrap();

    let (out, err) = call(
        dir.path(),
        "shell",
        json!({ "command": "echo progress; sleep 36.5", "timeout_secs": 1 }),
    )
    .await;

    assert!(err, "{out}");
    assert_eq!(
        out,
        "command timed out after 1 s and was killed\n--- stdout ---\nprogress"
    );
}

#[tokio::test]
async fn read_file_takes_a_huge_limit_without_failing() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.txt", "one\ntwo\n");

    let (out, err) = call(
        dir.path(),
        "read_file",
        json!({ "path": "a.txt", "offset": 2, "limit": u64::MAX }),
    )
    .await;

    assert!(!err, "{out}");
    assert_eq!(out, "     2\ttwo\n[lines 2-2 of 2]");
}

#[tokio::test]
async fn edit_file_matches_multi_line_text_in_a_crlf_file_and_keeps_crlf() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "win.txt", "alpha\r\nbeta\r\ngamma\r\n");

    let (out, err) = call(dir.path(), "edit_file", json!({ "path": "win.txt", "old_string": "alpha\nbeta", "new_string": "alpha\nBETA\nbeta2" })).await;

    assert!(!err, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("win.txt")).unwrap(),
        "alpha\r\nBETA\r\nbeta2\r\ngamma\r\n"
    );
}

#[tokio::test]
async fn cancelling_a_turn_kills_the_command_it_was_running() {
    let dir = tempfile::tempdir().unwrap();
    let marker = "sleep 38.75";
    let alive = || {
        std::process::Command::new("pgrep")
            .args(["-f", marker])
            .output()
            .unwrap()
            .status
            .success()
    };
    assert!(!alive(), "a `{marker}` process is already running");
    let transport = FakeTransport::new().respond(
        200,
        vec![&tool_reply(
            &[(
                "toolu_1",
                "shell",
                json!({ "command": format!("{marker} | cat"), "timeout_secs": 60 }),
            )],
            "tool_use",
        )],
    );
    let context = ToolContext::new(dir.path()).unwrap();
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        context,
    );
    let mut history = vec![user("go")];

    let cancelled = tokio::time::timeout(
        std::time::Duration::from_millis(800),
        agent.run_turn(&mut history, &mut (), &mut AllowAll),
    )
    .await;

    assert!(
        cancelled.is_err(),
        "the turn should still have been running"
    );
    std::thread::sleep(std::time::Duration::from_millis(300));
    assert!(!alive(), "`{marker}` outlived the cancelled turn");
}

#[tokio::test]
async fn search_stops_at_the_result_cap_and_says_so() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "many.txt", &"hit\n".repeat(700));

    let (out, err) = call(dir.path(), "search", json!({ "pattern": "hit" })).await;

    assert!(!err, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 501);
    assert_eq!(lines[499], "many.txt:500:hit");
    assert_eq!(lines[500], "[stopped at 500 results]");
}

#[tokio::test]
async fn edit_file_with_an_empty_old_string_fills_an_existing_empty_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "empty.py", "");

    let (out, err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": "empty.py", "old_string": "", "new_string": "print(1)\n" }),
    )
    .await;

    assert!(!err, "{out}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("empty.py")).unwrap(),
        "print(1)\n"
    );
}

#[tokio::test]
async fn read_and_edit_refuse_env_files_but_not_the_example() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), ".env", "ANTHROPIC_API_KEY=sk-ant-secret\n");
    write(dir.path(), "app/.env.local", "TOKEN=t\n");
    write(dir.path(), ".env.example", "ANTHROPIC_API_KEY=\n");
    std::os::unix::fs::symlink(dir.path().join(".env"), dir.path().join("settings.txt")).unwrap();

    for path in [".env", "app/.env.local", "settings.txt"] {
        let (out, err) = call(dir.path(), "read_file", json!({ "path": path })).await;
        assert!(err && out.contains("may hold secrets"), "{path}: {out}");
        assert!(!out.contains("sk-ant-secret"), "{path}: {out}");
    }
    let (edit, edit_err) = call(
        dir.path(),
        "edit_file",
        json!({ "path": ".env", "old_string": "sk-ant-secret", "new_string": "x" }),
    )
    .await;
    let (example, example_err) =
        call(dir.path(), "read_file", json!({ "path": ".env.example" })).await;

    assert!(edit_err && edit.contains("may hold secrets"), "{edit}");
    assert_eq!(
        std::fs::read_to_string(dir.path().join(".env")).unwrap(),
        "ANTHROPIC_API_KEY=sk-ant-secret\n"
    );
    assert!(!example_err, "{example}");
    assert_eq!(example, "     1\tANTHROPIC_API_KEY=");
}

/// Denies every call and records which tools it was asked about.
struct DenyAll(Vec<String>);

impl just_harness::agent::Approver for DenyAll {
    async fn approve(
        &mut self,
        request: &just_harness::agent::ApprovalRequest<'_>,
    ) -> just_harness::agent::Approval {
        self.0.push(request.name.to_string());
        just_harness::agent::Approval::Deny(None)
    }
}

#[tokio::test]
async fn shell_and_edit_file_ask_first_while_read_file_and_search_do_not() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.txt", "keep\n");
    let calls = [
        ("toolu_1", "read_file", json!({ "path": "a.txt" })),
        ("toolu_2", "search", json!({ "pattern": "keep" })),
        (
            "toolu_3",
            "edit_file",
            json!({ "path": "a.txt", "old_string": "keep", "new_string": "gone" }),
        ),
        ("toolu_4", "shell", json!({ "command": "touch ran.txt" })),
    ];
    let transport = FakeTransport::new()
        .respond(200, vec![&tool_reply(&calls, "tool_use")])
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let context = ToolContext::new(dir.path()).unwrap();
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        context,
    );
    let mut approver = DenyAll(Vec::new());

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut approver)
        .await
        .unwrap();

    assert_eq!(approver.0, vec!["edit_file", "shell"]);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "keep\n"
    );
    assert!(!dir.path().join("ran.txt").exists());
    let results = &sent_body(&transport, 1)["messages"][2]["content"];
    assert_eq!(results[0]["content"], "     1\tkeep");
    assert_eq!(results[1]["content"], "a.txt:1:keep");
}

/// Allows every call and records what each approval request showed.
struct Recorder(Vec<(String, String, String)>);

impl just_harness::agent::Approver for Recorder {
    async fn approve(
        &mut self,
        request: &just_harness::agent::ApprovalRequest<'_>,
    ) -> just_harness::agent::Approval {
        self.0.push((
            request.name.to_string(),
            request.preview.clone(),
            request.key.clone(),
        ));
        just_harness::agent::Approval::Allow
    }
}

#[tokio::test]
async fn approval_requests_show_the_command_or_the_edit_and_its_real_target() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "real.txt", "keep\n");
    std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("notes.txt")).unwrap();
    let calls = [
        (
            "toolu_1",
            "shell",
            json!({ "command": "echo hi", "timeout_secs": 5 }),
        ),
        (
            "toolu_2",
            "edit_file",
            json!({ "path": "notes.txt", "old_string": "keep", "new_string": "gone\nmore" }),
        ),
    ];
    let transport = FakeTransport::new()
        .respond(200, vec![&tool_reply(&calls, "tool_use")])
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let context = ToolContext::new(dir.path()).unwrap();
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        context,
    );
    let mut approver = Recorder(Vec::new());

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut approver)
        .await
        .unwrap();

    assert_eq!(
        approver.0[0],
        (
            "shell".into(),
            "$ echo hi\n(timeout 5 s)".into(),
            "echo hi".into()
        )
    );
    assert_eq!(
        approver.0[1],
        (
            "edit_file".into(),
            "notes.txt -> real.txt\n- keep\n+ gone\n+ more".into(),
            "notes.txt".into()
        )
    );
}
