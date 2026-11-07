//! The built-in tools, driven through the agent loop: a scripted reply calls
//! the tool, and the result is read from the next request.

mod support;

use std::path::Path;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
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

#[test]
fn a_folder_allowed_besides_the_working_folder_is_reachable_by_its_absolute_path() {
    let outer = tempfile::tempdir().unwrap();
    write(outer.path(), "work/a.txt", "a");
    write(outer.path(), "memory/MEMORY.md", "- note");
    write(outer.path(), "memory/.env", "KEY=1");
    write(outer.path(), "other/b.txt", "b");
    let memory = outer.path().join("memory");
    let context = ToolContext::new(outer.path().join("work"))
        .unwrap()
        .allow(&memory)
        .unwrap();
    let canonical = memory.canonicalize().unwrap();

    let index = canonical.join("MEMORY.md");
    assert_eq!(
        context.resolve_file(index.to_str().unwrap()),
        Ok(index.clone())
    );
    let new = canonical.join("role.md");
    assert_eq!(context.resolve_file(new.to_str().unwrap()), Ok(new));
    assert!(
        context
            .resolve_file(canonical.join(".env").to_str().unwrap())
            .is_err()
    );
    let other = outer.path().join("other/b.txt");
    assert!(
        context
            .resolve_file(other.to_str().unwrap())
            .unwrap_err()
            .contains("outside")
    );
    assert!(context.resolve_file("../memory/../other/b.txt").is_err());
    assert_eq!(context.allowed(), [canonical]);
}

/// The bytes of an opaque PNG; `noisy` ones do not compress.
fn png(width: u32, height: u32, noisy: bool) -> Vec<u8> {
    let mut seed = 1u32;
    let image = image::RgbImage::from_fn(width, height, |_, _| {
        if noisy {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [r, g, b, _] = seed.to_le_bytes();
            image::Rgb([r, g, b])
        } else {
            image::Rgb([128, 128, 128])
        }
    });
    encode(&image::DynamicImage::ImageRgb8(image))
}

fn encode(image: &image::DynamicImage) -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image.write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

/// Runs `read_file` on `path` in `root` and returns the user message that
/// carries the result.
async fn read_message(root: &Path, input: Value) -> Value {
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "read_file", input)], "tool_use")],
        )
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        ToolContext::new(root).unwrap(),
    );
    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();
    sent_body(&transport, 1)["messages"][2]["content"].clone()
}

fn decoded(source: &Value) -> Vec<u8> {
    BASE64
        .decode(source["data"].as_str().unwrap())
        .expect("base64 data")
}

#[tokio::test]
async fn read_file_returns_an_image_within_the_limits_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = png(4, 4, false);
    std::fs::write(dir.path().join("small.png"), &bytes).unwrap();

    let message = read_message(dir.path(), json!({ "path": "small.png" })).await;

    assert_eq!(
        message,
        json!([{
            "type": "tool_result",
            "tool_use_id": "toolu_1",
            "content": [{
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": "image/png",
                    "data": BASE64.encode(&bytes),
                },
            }],
        }])
    );
}

#[tokio::test]
async fn read_file_tells_an_image_by_its_bytes_not_its_extension() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["named.jpg", "no_extension", "UPPER.PNG"] {
        std::fs::write(dir.path().join(name), png(4, 4, false)).unwrap();
        let message = read_message(dir.path(), json!({ "path": name })).await;
        let block = &message[0]["content"][0];
        assert_eq!(block["type"], "image", "{name}");
        assert_eq!(block["source"]["media_type"], "image/png", "{name}");
    }
}

#[tokio::test]
async fn read_file_shrinks_a_long_edge_to_2000_and_says_so_after_the_results() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.png"), png(4000, 3000, false)).unwrap();
    std::fs::write(dir.path().join("edge.png"), png(2000, 100, false)).unwrap();

    let message = read_message(dir.path(), json!({ "path": "wide.png" })).await;

    let source = &message[0]["content"][0]["source"];
    assert_eq!(source["media_type"], "image/png");
    let sent = image::load_from_memory(&decoded(source)).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 1500));
    assert_eq!(
        message[1],
        json!({
            "type": "text",
            "text": "[Image: original 4000x3000, displayed at 2000x1500. Multiply coordinates by 2.00 to map to original image.]",
        })
    );
    assert_eq!(message.as_array().unwrap().len(), 2);

    let message = read_message(dir.path(), json!({ "path": "edge.png" })).await;
    assert_eq!(
        message.as_array().unwrap().len(),
        1,
        "2000 is within the limit"
    );
    let sent = image::load_from_memory(&decoded(&message[0]["content"][0]["source"])).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 100));
}

#[tokio::test]
async fn read_file_rounds_the_scaled_edge_to_the_nearest_pixel() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("just.png"), png(2001, 100, false)).unwrap();

    let message = read_message(dir.path(), json!({ "path": "just.png" })).await;

    let sent = image::load_from_memory(&decoded(&message[0]["content"][0]["source"])).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 100));
    assert_eq!(
        message[1]["text"],
        "[Image: original 2001x100, displayed at 2000x100. Multiply coordinates by 1.00 to map to original image.]"
    );
}

#[tokio::test]
async fn read_file_keeps_a_small_edge_at_one_pixel() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("line.png"), png(9000, 1, false)).unwrap();

    let message = read_message(dir.path(), json!({ "path": "line.png" })).await;

    let sent = image::load_from_memory(&decoded(&message[0]["content"][0]["source"])).unwrap();
    assert_eq!((sent.width(), sent.height()), (2000, 1));
}

#[tokio::test]
async fn read_file_sends_an_image_over_a_megabyte_as_jpeg_of_the_same_size() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = png(800, 800, true);
    assert!(bytes.len() > 1_000_000, "{}", bytes.len());
    std::fs::write(dir.path().join("noise.png"), &bytes).unwrap();

    let message = read_message(dir.path(), json!({ "path": "noise.png" })).await;

    let source = &message[0]["content"][0]["source"];
    assert_eq!(source["media_type"], "image/jpeg");
    let sent = decoded(source);
    assert!(sent.len() <= 1_000_000, "{}", sent.len());
    let sent = image::load_from_memory_with_format(&sent, image::ImageFormat::Jpeg).unwrap();
    assert_eq!((sent.width(), sent.height()), (800, 800));
    assert_eq!(message.as_array().unwrap().len(), 1, "nothing was resized");
}

#[tokio::test]
async fn read_file_keeps_an_image_of_at_most_a_megabyte_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let bytes = png(300, 300, true);
    assert!(bytes.len() < 1_000_000);
    std::fs::write(dir.path().join("noise.png"), &bytes).unwrap();

    let message = read_message(dir.path(), json!({ "path": "noise.png" })).await;

    assert_eq!(decoded(&message[0]["content"][0]["source"]), bytes);
}

#[tokio::test]
async fn read_file_puts_transparency_on_white_when_it_makes_a_jpeg() {
    let dir = tempfile::tempdir().unwrap();
    let mut seed = 1u32;
    let image = image::RgbaImage::from_fn(800, 800, |x, _| {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let [r, g, b, _] = seed.to_le_bytes();
        if x < 400 {
            image::Rgba([0, 0, 0, 0])
        } else {
            image::Rgba([r, g, b, 255])
        }
    });
    let bytes = encode(&image::DynamicImage::ImageRgba8(image));
    assert!(bytes.len() > 1_000_000, "{}", bytes.len());
    std::fs::write(dir.path().join("clear.png"), bytes).unwrap();

    let message = read_message(dir.path(), json!({ "path": "clear.png" })).await;

    let source = &message[0]["content"][0]["source"];
    assert_eq!(source["media_type"], "image/jpeg");
    let sent = image::load_from_memory(&decoded(source)).unwrap().to_rgb8();
    let pixel = sent.get_pixel(100, 400).0;
    assert!(pixel.iter().all(|&c| c > 240), "{pixel:?}");
}

#[tokio::test]
async fn read_file_puts_every_result_before_the_text_after_them() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("wide.png"), png(4000, 10, false)).unwrap();
    write(dir.path(), "a.txt", "alpha\n");
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(
                &[
                    ("t1", "read_file", json!({ "path": "wide.png" })),
                    ("t2", "read_file", json!({ "path": "a.txt" })),
                ],
                "tool_use",
            )],
        )
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let agent = Agent::new(
        Client::with_transport(config(), &transport),
        "claude-haiku-5-5",
        builtin(),
        ToolContext::new(dir.path()).unwrap(),
    );
    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();

    let message = &sent_body(&transport, 1)["messages"][2]["content"];
    let kinds: Vec<&str> = message
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["tool_result", "tool_result", "text"]);
}

#[tokio::test]
async fn read_file_refuses_what_is_not_the_image_its_name_says() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "login.png", "<html>log in</html>\n");
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "login.png" })).await;
    assert!(err, "{out}");
    assert!(out.contains("not a valid PNG/JPEG/GIF/WebP"), "{out}");

    write(dir.path(), "empty.png", "");
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "empty.png" })).await;
    assert!(err, "{out}");
    assert!(out.contains("empty"), "{out}");

    let mut cut = png(4, 4, false);
    cut.truncate(30);
    std::fs::write(dir.path().join("cut.png"), cut).unwrap();
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "cut.png" })).await;
    assert!(err, "{out}");
    assert!(out.contains("could not be decoded"), "{out}");
}

#[tokio::test]
async fn read_file_refuses_an_image_file_over_128_megabytes_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = std::fs::File::create(dir.path().join("huge.png")).unwrap();
    file.set_len(128 * 1024 * 1024 + 1).unwrap();

    let (out, err) = call(dir.path(), "read_file", json!({ "path": "huge.png" })).await;

    assert!(err, "{out}");
    assert!(out.contains("too large to read as an image"), "{out}");

    file.set_len(128 * 1024 * 1024).unwrap();
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "huge.png" })).await;
    assert!(err, "{out}");
    assert!(
        !out.contains("too large"),
        "128 MB itself is allowed: {out}"
    );
}

#[tokio::test]
async fn read_file_refuses_a_binary_format_by_its_extension() {
    let dir = tempfile::tempdir().unwrap();
    for name in ["a.zip", "b.bin", "c.HEIC", "d.dat", "e.xlsx", "f.parquet"] {
        // ASCII, so only the extension can be what refuses it.
        write(dir.path(), name, "plain\n");
        let (out, err) = call(dir.path(), "read_file", json!({ "path": name })).await;
        assert!(err, "{name}: {out}");
        assert!(out.contains("cannot read binary files"), "{name}: {out}");
    }
}

#[tokio::test]
async fn read_file_still_reads_text_under_an_unknown_extension_or_none() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "notes.unknownext", "hello\n");
    write(dir.path(), "README", "hi\n");
    let (out, err) = call(
        dir.path(),
        "read_file",
        json!({ "path": "notes.unknownext" }),
    )
    .await;
    assert_eq!((out.as_str(), err), ("     1\thello", false));
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "README" })).await;
    assert_eq!((out.as_str(), err), ("     1\thi", false));
}

#[tokio::test]
async fn read_file_keeps_a_text_extension_as_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("pic.txt"), png(4, 4, false)).unwrap();

    let (out, err) = call(dir.path(), "read_file", json!({ "path": "pic.txt" })).await;

    assert!(err, "{out}");
    assert!(out.contains("not UTF-8 text"), "{out}");
}

fn notebook(root: &Path, name: &str, notebook: &Value) {
    std::fs::write(root.join(name), notebook.to_string()).unwrap();
}

#[tokio::test]
async fn read_file_returns_a_notebooks_cells_with_their_outputs_and_images() {
    let dir = tempfile::tempdir().unwrap();
    let picture = BASE64.encode(png(4, 4, false));
    notebook(
        dir.path(),
        "n.ipynb",
        &json!({
            "cells": [
                { "cell_type": "markdown", "id": "md1", "metadata": {}, "source": ["# Title\n", "text"] },
                { "cell_type": "code", "id": "c1", "metadata": {}, "source": ["print(1)"],
                  "outputs": [{ "output_type": "stream", "name": "stdout", "text": ["1\n"] }] },
                { "cell_type": "code", "id": "c2", "metadata": {}, "source": ["plot()"],
                  "outputs": [{ "output_type": "display_data", "metadata": {},
                                "data": { "image/png": picture, "text/plain": ["<Figure>"] } }] },
            ],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
        }),
    );

    let message = read_message(dir.path(), json!({ "path": "n.ipynb" })).await;

    let content = &message[0]["content"];
    assert_eq!(content.as_array().unwrap().len(), 2);
    assert_eq!(
        content[0],
        json!({
            "type": "text",
            "text": "<cell id=\"md1\"><cell_type>markdown</cell_type># Title\ntext</cell id=\"md1\">\n<cell id=\"c1\">print(1)</cell id=\"c1\">\n\n1\n\n<cell id=\"c2\">plot()</cell id=\"c2\">\n\n<Figure>",
        })
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
}

#[tokio::test]
async fn read_file_handles_a_notebooks_other_languages_ids_and_output_kinds() {
    let dir = tempfile::tempdir().unwrap();
    // The data is a PNG whatever key it is under.
    let picture = BASE64.encode(png(4, 4, false));
    notebook(
        dir.path(),
        "edge.ipynb",
        &json!({
            "cells": [
                { "cell_type": "code", "metadata": {}, "source": "no_id()",
                  "outputs": [{ "output_type": "execute_result", "metadata": {},
                                "data": { "text/plain": ["42"] } }] },
                { "cell_type": "raw", "metadata": {}, "source": ["raw text"] },
                { "cell_type": "code", "id": "e1", "metadata": {}, "source": ["boom()"],
                  "outputs": [{ "output_type": "error", "ename": "ValueError", "evalue": "bad",
                                "traceback": ["tb line 1", "tb line 2"] }] },
                { "cell_type": "code", "id": "t1", "metadata": {}, "source": ["long()"],
                  "outputs": [{ "output_type": "stream", "name": "stdout", "text": "x".repeat(20_000) }] },
                { "cell_type": "code", "id": "j1", "metadata": {}, "source": ["both()"],
                  "outputs": [
                      { "output_type": "display_data", "metadata": {},
                        "data": { "image/jpeg": picture, "text/html": ["<b>h</b>"] } },
                      { "output_type": "stream", "name": "stderr", "text": ["warn\n"] },
                  ] },
            ],
            "metadata": { "language_info": { "name": "julia" } },
            "nbformat": 4, "nbformat_minor": 4,
        }),
    );

    let message = read_message(dir.path(), json!({ "path": "edge.ipynb" })).await;

    let content = &message[0]["content"];
    assert_eq!(content.as_array().unwrap().len(), 3);
    assert_eq!(
        content[0]["text"],
        "<cell id=\"cell-0\"><language>julia</language>no_id()</cell id=\"cell-0\">\n\n42\n<cell id=\"cell-1\"><cell_type>raw</cell_type>raw text</cell id=\"cell-1\">\n<cell id=\"e1\"><language>julia</language>boom()</cell id=\"e1\">\n\nValueError: bad\ntb line 1\ntb line 2\n<cell id=\"t1\"><language>julia</language>long()</cell id=\"t1\">\n\nOutputs are too large to include. Use a shell command: cat <notebook_path> | jq '.cells[3].outputs'\n<cell id=\"j1\"><language>julia</language>both()</cell id=\"j1\">"
    );
    assert_eq!(content[1]["type"], "image");
    assert_eq!(content[1]["source"]["media_type"], "image/png");
    assert_eq!(content[2]["text"], "\nwarn\n");
}

#[tokio::test]
async fn read_file_keeps_a_notebooks_outputs_of_at_most_10000_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let cell = |n: usize| {
        json!({
            "cells": [{ "cell_type": "code", "id": "c", "metadata": {}, "source": [],
                        "outputs": [
                            { "output_type": "stream", "name": "stdout", "text": "x".repeat(n / 2) },
                            { "output_type": "stream", "name": "stderr", "text": "y".repeat(n - n / 2) },
                        ] }],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
        })
    };
    notebook(dir.path(), "fits.ipynb", &cell(10_000));
    notebook(dir.path(), "over.ipynb", &cell(10_001));

    let fits = read_message(dir.path(), json!({ "path": "fits.ipynb" })).await;
    let over = read_message(dir.path(), json!({ "path": "over.ipynb" })).await;

    assert!(
        !fits[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("too large")
    );
    assert!(
        over[0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("too large")
    );
}

#[tokio::test]
async fn read_file_keeps_the_text_of_an_output_whose_image_does_not_decode() {
    let dir = tempfile::tempdir().unwrap();
    notebook(
        dir.path(),
        "bad.ipynb",
        &json!({
            "cells": [{ "cell_type": "code", "id": "c", "metadata": {}, "source": [],
                        "outputs": [{ "output_type": "display_data", "metadata": {},
                                      "data": { "image/png": "bm90IGEgcGljdHVyZQ==", "text/plain": "kept" } }] }],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
        }),
    );

    let message = read_message(dir.path(), json!({ "path": "bad.ipynb" })).await;

    let content = &message[0]["content"];
    assert_eq!(content.as_array().unwrap().len(), 1, "{content}");
    let text = content[0]["text"].as_str().unwrap();
    assert!(
        text.contains("kept") && text.contains("[an image output"),
        "{text}"
    );
}

#[tokio::test]
async fn read_file_decodes_notebook_images_split_over_lines() {
    let dir = tempfile::tempdir().unwrap();
    let picture = BASE64.encode(png(4, 4, false));
    let (head, tail) = picture.split_at(20);
    notebook(
        dir.path(),
        "split.ipynb",
        &json!({
            "cells": [{ "cell_type": "code", "id": "c", "metadata": {}, "source": [],
                        "outputs": [{ "output_type": "display_data", "metadata": {},
                                      "data": { "image/png": format!("{head}\n{tail}\n") } }] }],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
        }),
    );

    let message = read_message(dir.path(), json!({ "path": "split.ipynb" })).await;

    assert_eq!(message[0]["content"][1]["type"], "image");
}

#[tokio::test]
async fn read_file_refuses_a_notebook_it_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "broken.ipynb", "{not json");
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "broken.ipynb" })).await;
    assert!(err, "{out}");
    assert!(out.contains("not valid JSON"), "{out}");

    write(dir.path(), "nocells.ipynb", "{\"metadata\":{}}");
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "nocells.ipynb" })).await;
    assert!(err, "{out}");
    assert!(out.contains("not a valid Jupyter notebook"), "{out}");

    write(
        dir.path(),
        "nosource.ipynb",
        "{\"cells\":[{\"cell_type\":\"code\",\"source\":1}]}",
    );
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "nosource.ipynb" })).await;
    assert!(err, "{out}");
    assert!(out.contains("not a valid Jupyter notebook"), "{out}");
}

#[tokio::test]
async fn read_file_refuses_a_notebook_over_its_size_limits() {
    let dir = tempfile::tempdir().unwrap();
    let cells = |n: usize| {
        json!({
            "cells": [{ "cell_type": "markdown", "id": "b", "metadata": {}, "source": ["y".repeat(n)] }],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5,
        })
    };
    let wrapper = "<cell id=\"b\"><cell_type>markdown</cell_type></cell id=\"b\">".len();
    notebook(dir.path(), "fits.ipynb", &cells(256 * 1024 - wrapper));
    notebook(dir.path(), "over.ipynb", &cells(256 * 1024 - wrapper + 1));

    let message = read_message(dir.path(), json!({ "path": "fits.ipynb" })).await;
    assert_eq!(message[0].get("is_error"), None);

    let (out, err) = call(dir.path(), "read_file", json!({ "path": "over.ipynb" })).await;
    assert!(err, "{out}");
    assert!(
        out.contains("exceeds maximum allowed size (256KB)"),
        "{out}"
    );

    let file = std::fs::File::create(dir.path().join("huge.ipynb")).unwrap();
    file.set_len(100 * 1024 * 1024 + 1).unwrap();
    let (out, err) = call(dir.path(), "read_file", json!({ "path": "huge.ipynb" })).await;
    assert!(err, "{out}");
    assert!(out.contains("too large"), "{out}");
}

/// A PDF of `pages` empty pages of 200x200 points.
fn pdf(pages: usize) -> Vec<u8> {
    let kids: Vec<String> = (0..pages).map(|i| format!("{} 0 R", 3 + i)).collect();
    let mut objects = vec![
        "<</Type/Catalog/Pages 2 0 R>>".to_string(),
        format!("<</Type/Pages/Kids[{}]/Count {pages}>>", kids.join(" ")),
    ];
    for _ in 0..pages {
        objects.push("<</Type/Page/Parent 2 0 R/MediaBox[0 0 200 200]>>".to_string());
    }
    let mut out = String::from("%PDF-1.4\n");
    let mut offsets = Vec::new();
    for (i, object) in objects.iter().enumerate() {
        offsets.push(out.len());
        out += &format!("{} 0 obj{object}endobj\n", i + 1);
    }
    let table = out.len();
    out += &format!("xref\n0 {}\n0000000000 65535 f \n", objects.len() + 1);
    for offset in offsets {
        out += &format!("{offset:010} 00000 n \n");
    }
    out += &format!(
        "trailer<</Size {}/Root 1 0 R>>\nstartxref\n{table}\n%%EOF\n",
        objects.len() + 1
    );
    out.into_bytes()
}

/// Whether poppler's tools are here; tests of what they do say so when not.
fn poppler() -> bool {
    let found = ["pdfinfo", "pdftoppm"]
        .iter()
        .all(|tool| std::process::Command::new(tool).arg("-v").output().is_ok());
    if !found {
        eprintln!("skipped: pdfinfo and pdftoppm are not installed");
    }
    found
}

fn kinds(content: &Value) -> Vec<&str> {
    content
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["type"].as_str().unwrap())
        .collect()
}

#[tokio::test]
async fn read_file_returns_a_short_pdf_whole_as_a_document() {
    if !poppler() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bytes = pdf(10);
    std::fs::write(dir.path().join("ten.PDF"), &bytes).unwrap();

    let message = read_message(dir.path(), json!({ "path": "ten.PDF" })).await;

    let content = &message[0]["content"];
    assert_eq!(kinds(content), ["text", "document"]);
    let text = content[0]["text"].as_str().unwrap();
    assert!(text.starts_with("PDF file read: ten.PDF ("), "{text}");
    assert_eq!(
        content[1],
        json!({
            "type": "document",
            "source": {
                "type": "base64",
                "media_type": "application/pdf",
                "data": BASE64.encode(&bytes),
            },
        })
    );
}

#[tokio::test]
async fn read_file_asks_for_pages_of_a_longer_pdf() {
    if !poppler() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("eleven.pdf"), pdf(11)).unwrap();

    let (out, err) = call(dir.path(), "read_file", json!({ "path": "eleven.pdf" })).await;

    assert!(err, "{out}");
    assert_eq!(
        out,
        "This PDF has 11 pages, which is too many to read at once. Use the pages parameter to read specific page ranges (e.g., pages: \"1-5\"). Maximum 20 pages per request."
    );
}

#[tokio::test]
async fn read_file_renders_the_pages_asked_for_as_jpegs_at_100_dpi() {
    if !poppler() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("long.pdf"), pdf(12)).unwrap();

    let message = read_message(dir.path(), json!({ "path": "long.pdf", "pages": "2-3" })).await;

    let content = &message[0]["content"];
    assert_eq!(kinds(content), ["text", "image", "image"]);
    let text = content[0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("PDF pages extracted: 2 page(s) from long.pdf ("),
        "{text}"
    );
    let source = &content[1]["source"];
    assert_eq!(source["media_type"], "image/jpeg");
    let page = image::load_from_memory(&decoded(source)).unwrap();
    assert_eq!((page.width(), page.height()), (278, 278));

    let message = read_message(dir.path(), json!({ "path": "long.pdf", "pages": "12" })).await;
    assert_eq!(kinds(&message[0]["content"]), ["text", "image"]);
}

#[tokio::test]
async fn read_file_cuts_a_page_range_at_the_last_page() {
    if !poppler() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("three.pdf"), pdf(3)).unwrap();

    let message = read_message(dir.path(), json!({ "path": "three.pdf", "pages": "2-5" })).await;
    assert_eq!(kinds(&message[0]["content"]), ["text", "image", "image"]);

    let (out, err) = call(
        dir.path(),
        "read_file",
        json!({ "path": "three.pdf", "pages": "5-6" }),
    )
    .await;
    assert!(err, "{out}");
    assert_eq!(
        out,
        "Requested pages 5-6 is outside the document (PDF has 3 pages). Use a range within 1-3, maximum 20 pages per request (e.g. pages: \"1-3\")."
    );
}

#[tokio::test]
async fn read_file_allows_twenty_pages_and_refuses_more() {
    if !poppler() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("long.pdf"), pdf(21)).unwrap();

    let message = read_message(dir.path(), json!({ "path": "long.pdf", "pages": "1-20" })).await;
    assert_eq!(message[0]["content"].as_array().unwrap().len(), 21);

    let (out, err) = call(
        dir.path(),
        "read_file",
        json!({ "path": "long.pdf", "pages": "1-21" }),
    )
    .await;
    assert!(err, "{out}");
    assert_eq!(
        out,
        "Page range \"1-21\" exceeds maximum of 20 pages per request. Please use a smaller range."
    );
}

#[tokio::test]
async fn read_file_refuses_pages_it_cannot_parse() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("three.pdf"), pdf(3)).unwrap();

    for pages in ["abc", "3-1", "0", "1,2", "1-", "-2", ""] {
        let (out, err) = call(
            dir.path(),
            "read_file",
            json!({ "path": "three.pdf", "pages": pages }),
        )
        .await;
        assert!(err, "{pages}: {out}");
        assert_eq!(
            out,
            format!(
                "Invalid pages parameter: \"{pages}\". Give one page (\"3\") or one range (\"1-5\"), not a list. To read several pages or ranges, read each one separately. Pages are 1-indexed."
            )
        );
    }
}

#[tokio::test]
async fn read_file_refuses_what_is_not_the_pdf_its_name_says() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "fake.pdf", "not a pdf\n");

    let (out, err) = call(dir.path(), "read_file", json!({ "path": "fake.pdf" })).await;

    assert!(err, "{out}");
    assert_eq!(
        out,
        "File is not a valid PDF (missing %PDF- header): fake.pdf"
    );
}

#[tokio::test]
async fn read_file_refuses_a_pdf_too_large_to_send_whole_without_reading_it() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = std::fs::File::create(dir.path().join("huge.pdf")).unwrap();
    std::io::Write::write_all(&mut file, b"%PDF-1.4\n").unwrap();
    file.set_len(90_000_001).unwrap();

    let (out, err) = call(dir.path(), "read_file", json!({ "path": "huge.pdf" })).await;

    assert!(err, "{out}");
    assert!(out.contains("too large to send whole"), "{out}");
}

#[tokio::test]
async fn read_file_ignores_pages_on_a_text_file() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.txt", "hello\n");

    let (out, err) = call(
        dir.path(),
        "read_file",
        json!({ "path": "a.txt", "pages": "abc" }),
    )
    .await;

    assert_eq!((out.as_str(), err), ("     1\thello", false));
}

#[tokio::test]
async fn read_file_gives_a_pdfs_size_in_kilobytes_without_a_bare_decimal() {
    let dir = tempfile::tempdir().unwrap();
    for (bytes, shown) in [(1024, "(1KB)"), (3584, "(3.5KB)")] {
        let mut data = b"%PDF-1.4\n".to_vec();
        data.resize(bytes, b' ');
        std::fs::write(dir.path().join("sized.pdf"), data).unwrap();
        let message = read_message(dir.path(), json!({ "path": "sized.pdf" })).await;
        let text = message[0]["content"][0]["text"].as_str().unwrap();
        assert!(text.ends_with(shown), "{text}");
    }
}
