mod support;

use std::sync::Mutex;

use just_harness::agent::{Agent, AllowAll, Approval, ApprovalRequest, Approver, TurnEnd};
use just_harness::api::{self, Client, StopReason};
use just_harness::tools::{Tool, ToolContext, ToolFuture, ToolOutput, ToolSet};
use serde_json::{Value, json};
use support::{FakeTransport, config, sent_body, text_reply, tool_reply, user};

/// A tool that records its inputs and answers with a fixed output.
struct Echo {
    name: &'static str,
    output: ToolOutput,
    calls: Mutex<Vec<Value>>,
    needs_approval: bool,
}

impl Echo {
    fn new(name: &'static str, output: ToolOutput) -> Self {
        Self {
            name,
            output,
            calls: Mutex::new(Vec::new()),
            needs_approval: false,
        }
    }
}

impl Tool for &Echo {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: self.name.into(),
            description: "test tool".into(),
            input_schema: json!({ "type": "object" }),
            cache_control: None,
        }
    }

    fn call<'a>(&'a self, input: &'a Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
        self.calls.lock().unwrap().push(input.clone());
        let output = self.output.clone();
        Box::pin(async move { output })
    }
    fn needs_approval(&self) -> bool {
        self.needs_approval
    }
}

fn agent(transport: &FakeTransport, tools: ToolSet) -> (Agent<&FakeTransport>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let context = ToolContext::new(dir.path()).unwrap();
    let agent = Agent::new(
        Client::with_transport(config(), transport),
        "claude-haiku-5-5",
        tools,
        context,
    );
    (agent, dir)
}

#[tokio::test]
async fn runs_a_requested_tool_and_sends_its_result_back() {
    let echo: &'static Echo = Box::leak(Box::new(Echo::new("lookup", ToolOutput::ok("42"))));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(
                &[("toolu_1", "lookup", json!({ "q": "answer" }))],
                "tool_use",
            )],
        )
        .respond(200, vec![&text_reply("It is 42.", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut history = vec![user("What is the answer?")];

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::EndTurn)));
    assert_eq!(outcome.iterations, 2);
    assert_eq!(*echo.calls.lock().unwrap(), vec![json!({ "q": "answer" })]);
    assert_eq!(history.len(), 4);
    let second = sent_body(&transport, 1);
    assert_eq!(
        second["messages"][2],
        json!({ "role": "user", "content": [{ "type": "tool_result", "tool_use_id": "toolu_1", "content": "42" }] })
    );
    assert_eq!(second["tools"][0]["name"], "lookup");
}

fn leak(echo: Echo) -> &'static Echo {
    Box::leak(Box::new(echo))
}

#[tokio::test]
async fn returns_every_parallel_result_in_one_message_in_call_order() {
    let a = leak(Echo::new("a", ToolOutput::ok("from a")));
    let b = leak(Echo::new("b", ToolOutput::error("b failed")));
    let calls = [
        ("toolu_1", "b", json!({})),
        ("toolu_2", "a", json!({})),
        ("toolu_3", "missing", json!({})),
    ];
    let transport = FakeTransport::new()
        .respond(200, vec![&tool_reply(&calls, "tool_use")])
        .respond(200, vec![&text_reply("done", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(a).with(b));
    let mut history = vec![user("go")];

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(
        sent_body(&transport, 1)["messages"][2],
        json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "b failed", "is_error": true },
            { "type": "tool_result", "tool_use_id": "toolu_2", "content": "from a" },
            { "type": "tool_result", "tool_use_id": "toolu_3", "content": "Error: Tool 'missing' not found", "is_error": true }
        ] })
    );
}

#[tokio::test]
async fn answers_tool_calls_cut_by_max_tokens_with_not_run_errors_instead_of_running_them() {
    let echo = leak(Echo::new("lookup", ToolOutput::ok("x")));
    let calls = [
        ("toolu_1", "lookup", json!({})),
        ("toolu_2", "lookup", json!({})),
    ];
    let transport = FakeTransport::new().respond(200, vec![&tool_reply(&calls, "max_tokens")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut history = vec![user("go")];

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::MaxTokens)));
    assert!(echo.calls.lock().unwrap().is_empty());
    assert_eq!(transport.requests().len(), 1);
    assert_eq!(history.len(), 3);
    assert_eq!(
        serde_json::to_value(&history[2]).unwrap(),
        json!({ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "Not run: the reply stopped at max_tokens.", "is_error": true },
            { "type": "tool_result", "tool_use_id": "toolu_2", "content": "Not run: the reply stopped at max_tokens.", "is_error": true }
        ] })
    );
}

#[tokio::test]
async fn a_finished_reply_without_tool_calls_adds_nothing_after_it() {
    let transport = FakeTransport::new().respond(200, vec![&text_reply("hi", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new());
    let mut history = vec![user("go")];

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(history.len(), 2);
}

#[tokio::test]
async fn resends_a_paused_turn_unchanged_without_running_tools() {
    let echo = leak(Echo::new("lookup", ToolOutput::ok("x")));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(
                &[("toolu_1", "lookup", json!({}))],
                "pause_turn",
            )],
        )
        .respond(200, vec![&text_reply("done", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut history = vec![user("go")];

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::EndTurn)));
    assert!(echo.calls.lock().unwrap().is_empty());
    let messages = &sent_body(&transport, 1)["messages"];
    assert_eq!(messages.as_array().unwrap().len(), 2);
    assert_eq!(messages[1]["role"], "assistant");
}

#[tokio::test]
async fn stops_at_the_iteration_cap() {
    let echo = leak(Echo::new("lookup", ToolOutput::ok("again")));
    let reply = tool_reply(&[("toolu_1", "lookup", json!({}))], "tool_use");
    let transport = FakeTransport::new()
        .respond(200, vec![&reply])
        .respond(200, vec![&reply])
        .respond(200, vec![&reply]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    agent.max_iterations = 3;
    let mut history = vec![user("go")];

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::IterationLimit);
    assert_eq!(outcome.iterations, 3);
    assert_eq!(transport.requests().len(), 3);
    assert_eq!(echo.calls.lock().unwrap().len(), 3);
    assert_eq!(history.last().unwrap().role, just_harness::api::Role::User);
}

#[tokio::test]
async fn each_request_extends_the_previous_one_without_changing_it() {
    let echo = leak(Echo::new("lookup", ToolOutput::ok("x")));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(
                &[("toolu_1", "lookup", json!({ "n": 1 }))],
                "tool_use",
            )],
        )
        .respond(
            200,
            vec![&tool_reply(
                &[("toolu_2", "lookup", json!({ "n": 2 }))],
                "tool_use",
            )],
        )
        .respond(200, vec![&text_reply("done", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut history = vec![user("go")];

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    let bodies: Vec<Value> = (0..3).map(|n| sent_body(&transport, n)).collect();
    for pair in bodies.windows(2) {
        let (before, after) = (
            pair[0]["messages"].as_array().unwrap(),
            pair[1]["messages"].as_array().unwrap(),
        );
        assert_eq!(&after[..before.len()], &before[..]);
        assert_eq!(after.len(), before.len() + 2);
    }
    assert_eq!(history.len(), 6);
}

#[tokio::test]
async fn sends_the_assistant_turn_back_with_its_thinking_block_unchanged() {
    use support::{message_end, message_start, sse};
    let echo = leak(Echo::new("lookup", ToolOutput::ok("x")));
    let reply = [
        message_start(10),
        sse(json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "thinking", "thinking": "", "signature": "" } })),
        sse(json!({ "type": "content_block_delta", "index": 0, "delta": { "type": "signature_delta", "signature": "c2ln" } })),
        sse(json!({ "type": "content_block_stop", "index": 0 })),
        sse(json!({ "type": "content_block_start", "index": 1,
            "content_block": { "type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {} } })),
        sse(json!({ "type": "content_block_stop", "index": 1 })),
        message_end("tool_use", 5),
    ]
    .concat();
    let transport = FakeTransport::new()
        .respond(200, vec![&reply])
        .respond(200, vec![&text_reply("done", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut history = vec![user("go")];

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(
        sent_body(&transport, 1)["messages"][1],
        json!({ "role": "assistant", "content": [
            { "type": "thinking", "thinking": "", "signature": "c2ln" },
            { "type": "tool_use", "id": "toolu_1", "name": "lookup", "input": {} }
        ] })
    );
}

struct Panics;

impl Tool for Panics {
    fn definition(&self) -> api::Tool {
        api::Tool {
            name: "boom".into(),
            description: "panics".into(),
            input_schema: json!({ "type": "object" }),
            cache_control: None,
        }
    }

    fn call<'a>(&'a self, _input: &'a Value, _ctx: &'a ToolContext) -> ToolFuture<'a> {
        Box::pin(async { panic!("index out of range") })
    }

    fn needs_approval(&self) -> bool {
        false
    }
}

#[tokio::test]
async fn a_panicking_tool_becomes_an_error_result_and_the_loop_goes_on() {
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "boom", json!({}))], "tool_use")],
        )
        .respond(200, vec![&text_reply("recovered", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(Panics));
    let mut history = vec![user("go")];

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::EndTurn)));
    assert_eq!(
        sent_body(&transport, 1)["messages"][2]["content"][0],
        json!({ "type": "tool_result", "tool_use_id": "toolu_1", "content": "Error: tool 'boom' panicked: index out of range", "is_error": true })
    );
}

fn guarded(name: &'static str) -> &'static Echo {
    let mut echo = Echo::new(name, ToolOutput::ok("done"));
    echo.needs_approval = true;
    leak(echo)
}

/// Answers approvals from a script and records what it was asked.
struct Scripted {
    answers: std::collections::VecDeque<Approval>,
    asked: Vec<(String, Value)>,
}

impl Scripted {
    fn new(answers: Vec<Approval>) -> Self {
        Self {
            answers: answers.into(),
            asked: Vec::new(),
        }
    }
}

impl Approver for Scripted {
    async fn approve(&mut self, request: &ApprovalRequest<'_>) -> Approval {
        self.asked
            .push((request.name.to_string(), request.input.clone()));
        self.answers
            .pop_front()
            .expect("asked more often than scripted")
    }
}

#[tokio::test]
async fn asks_only_about_tools_that_need_approval_and_runs_what_is_allowed() {
    let read = leak(Echo::new("look", ToolOutput::ok("seen")));
    let write = guarded("change");
    let calls = [
        ("toolu_1", "look", json!({ "a": 1 })),
        ("toolu_2", "change", json!({ "b": 2 })),
    ];
    let transport = FakeTransport::new()
        .respond(200, vec![&tool_reply(&calls, "tool_use")])
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(read).with(write));
    let mut approver = Scripted::new(vec![Approval::Allow]);

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut approver)
        .await
        .unwrap();

    assert_eq!(
        approver.asked,
        vec![("change".to_string(), json!({ "b": 2 }))]
    );
    assert_eq!(read.calls.lock().unwrap().len(), 1);
    assert_eq!(write.calls.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn a_denied_call_is_not_run_and_the_reason_reaches_the_model() {
    let write = guarded("change");
    let calls = [
        ("toolu_1", "change", json!({})),
        ("toolu_2", "change", json!({})),
    ];
    let transport = FakeTransport::new()
        .respond(200, vec![&tool_reply(&calls, "tool_use")])
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(write));
    let mut approver = Scripted::new(vec![
        Approval::Deny(Some("run the tests instead".into())),
        Approval::Deny(None),
    ]);

    let outcome = agent
        .run_turn(&mut vec![user("go")], &mut (), &mut approver)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::EndTurn)));
    assert!(write.calls.lock().unwrap().is_empty());
    assert_eq!(
        sent_body(&transport, 1)["messages"][2]["content"],
        json!([
            { "type": "tool_result", "tool_use_id": "toolu_1", "content": "The user denied this call. Reason: run the tests instead", "is_error": true },
            { "type": "tool_result", "tool_use_id": "toolu_2", "content": "The user denied this call.", "is_error": true }
        ])
    );
}

#[tokio::test]
async fn stop_skips_the_remaining_calls_and_ends_the_turn_after_one_more_reply() {
    let write = guarded("change");
    let calls = [
        ("toolu_1", "change", json!({})),
        ("toolu_2", "change", json!({})),
    ];
    let transport = FakeTransport::new()
        .respond(200, vec![&tool_reply(&calls, "tool_use")])
        .respond(
            200,
            vec![&tool_reply(&[("toolu_3", "change", json!({}))], "tool_use")],
        );
    let (agent, _dir) = agent(&transport, ToolSet::new().with(write));
    let mut approver = Scripted::new(vec![Approval::Stop]);
    let mut history = vec![user("go")];

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut approver)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::StoppedByUser);
    assert_eq!(approver.asked.len(), 1);
    assert!(write.calls.lock().unwrap().is_empty());
    assert_eq!(transport.requests().len(), 2);
    let not_run = |id: &str| json!({ "type": "tool_result", "tool_use_id": id, "content": "Not run: the user stopped the turn.", "is_error": true });
    assert_eq!(
        sent_body(&transport, 1)["messages"][2]["content"],
        json!([not_run("toolu_1"), not_run("toolu_2")])
    );
    assert_eq!(history.len(), 5);
    assert_eq!(
        serde_json::to_value(&history[4]).unwrap()["content"],
        json!([not_run("toolu_3")])
    );
}

#[tokio::test]
async fn stop_on_the_last_allowed_request_still_gets_its_one_more_reply() {
    let write = guarded("change");
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "change", json!({}))], "tool_use")],
        )
        .respond(200, vec![&text_reply("stopping", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new().with(write));
    agent.max_iterations = 1;
    let mut approver = Scripted::new(vec![Approval::Stop]);

    let outcome = agent
        .run_turn(&mut vec![user("go")], &mut (), &mut approver)
        .await
        .unwrap();

    assert_eq!(outcome.end, TurnEnd::StoppedByUser);
    assert_eq!(transport.requests().len(), 2);
}

#[tokio::test]
async fn sends_the_agents_extra_request_fields_with_every_request() {
    let echo = leak(Echo::new("look", ToolOutput::ok("seen")));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "look", json!({}))], "tool_use")],
        )
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    agent
        .extra
        .insert("output_config".into(), json!({ "effort": "high" }));

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();

    for n in 0..2 {
        assert_eq!(
            sent_body(&transport, n)["output_config"],
            json!({ "effort": "high" })
        );
    }
}

/// Keeps every message the loop reports appending.
#[derive(Default)]
struct Appended(Vec<just_harness::api::MessageParam>);

impl just_harness::agent::Observer for Appended {
    fn on_message(&mut self, message: &just_harness::api::MessageParam) {
        self.0.push(message.clone());
    }
}

#[tokio::test]
async fn reports_every_message_it_appends_in_order() {
    let echo = leak(Echo::new("look", ToolOutput::ok("seen")));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "look", json!({}))], "tool_use")],
        )
        .respond(
            200,
            vec![&tool_reply(&[("toolu_2", "look", json!({}))], "max_tokens")],
        );
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut history = vec![user("go")];
    let mut appended = Appended::default();

    agent
        .run_turn(&mut history, &mut appended, &mut AllowAll)
        .await
        .unwrap();

    assert_eq!(history.len(), 5);
    assert_eq!(appended.0, history[1..].to_vec());
}

#[tokio::test]
async fn reports_the_not_run_results_of_a_stopped_turn() {
    let write = guarded("change");
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "change", json!({}))], "tool_use")],
        )
        .respond(
            200,
            vec![&tool_reply(&[("toolu_2", "change", json!({}))], "tool_use")],
        );
    let (agent, _dir) = agent(&transport, ToolSet::new().with(write));
    let mut history = vec![user("go")];
    let mut appended = Appended::default();

    agent
        .run_turn(
            &mut history,
            &mut appended,
            &mut Scripted::new(vec![Approval::Stop]),
        )
        .await
        .unwrap();

    assert_eq!(history.len(), 5);
    assert_eq!(appended.0, history[1..].to_vec());
}

#[tokio::test]
async fn marks_the_system_prompt_and_asks_for_automatic_caching_of_the_rest() {
    let transport = FakeTransport::new().respond(200, vec![&text_reply("ok", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new());
    agent.system = vec![
        just_harness::api::ContentBlock::text("part one"),
        just_harness::api::ContentBlock::text("part two"),
    ];

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();

    let body = sent_body(&transport, 0);
    assert_eq!(body["cache_control"], json!({ "type": "ephemeral" }));
    assert_eq!(
        body["system"][1]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert!(body["system"][0].get("cache_control").is_none());
    assert_eq!(body.to_string().matches("cache_control").count(), 2);
    assert!(
        agent.system[1].0.get("cache_control").is_none(),
        "the agent's own system blocks stay unmarked"
    );
}

#[tokio::test]
async fn sends_no_cache_control_when_caching_is_off() {
    let transport = FakeTransport::new().respond(200, vec![&text_reply("ok", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new());
    agent.system = vec![just_harness::api::ContentBlock::text("system")];
    agent.prompt_cache = false;

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert!(
        !sent_body(&transport, 0)
            .to_string()
            .contains("cache_control")
    );
}

#[tokio::test]
async fn keeps_tools_and_system_byte_identical_across_requests() {
    let echo = leak(Echo::new("look", ToolOutput::ok("seen")));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "look", json!({}))], "tool_use")],
        )
        .respond(200, vec![&text_reply("ok", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    agent.system = vec![just_harness::api::ContentBlock::text("system")];

    agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();

    let section = |n: usize, key: &str| sent_body(&transport, n)[key].to_string();
    assert_eq!(
        sent_body(&transport, 0)["tools"].as_array().unwrap().len(),
        1
    );
    for key in ["tools", "system", "model"] {
        assert_eq!(
            section(0, key),
            section(1, key),
            "{key} changed between requests"
        );
    }
}

#[tokio::test]
async fn sums_usage_over_the_requests_of_a_turn() {
    use support::{message_start, sse};
    let echo = leak(Echo::new("look", ToolOutput::ok("seen")));
    let reply = |input: u64, read: u64, write: u64, out: u64, stop: &str| {
        let mut start = message_start(input);
        start = start.replace(
            &format!("\"input_tokens\":{input}"),
            &format!("\"input_tokens\":{input},\"cache_read_input_tokens\":{read},\"cache_creation_input_tokens\":{write}"),
        );
        let block = if stop == "tool_use" {
            sse(
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "tool_use", "id": "toolu_1", "name": "look", "input": {} } }),
            )
        } else {
            sse(
                json!({ "type": "content_block_start", "index": 0, "content_block": { "type": "text", "text": "ok" } }),
            )
        };
        start
            + &block
            + &sse(json!({ "type": "content_block_stop", "index": 0 }))
            + &sse(
                json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null }, "usage": { "output_tokens": out } }),
            )
            + &sse(json!({ "type": "message_stop" }))
    };
    let transport = FakeTransport::new()
        .respond(200, vec![&reply(10, 0, 1400, 30, "tool_use")])
        .respond(200, vec![&reply(20, 1400, 50, 7, "end_turn")]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));

    let outcome = agent
        .run_turn(&mut vec![user("go")], &mut (), &mut AllowAll)
        .await
        .unwrap();

    let usage = &outcome.usage;
    assert_eq!((usage.input_tokens, usage.output_tokens), (30, 37));
    assert_eq!(usage.cache_read_input_tokens, Some(1400));
    assert_eq!(usage.cache_creation_input_tokens, Some(1450));
}

/// Keeps the usage of every request the loop reports.
#[derive(Default)]
struct Usages(Vec<just_harness::api::Usage>);

impl just_harness::agent::Observer for Usages {
    fn on_usage(&mut self, usage: &just_harness::api::Usage) {
        self.0.push(usage.clone());
    }
}

#[tokio::test]
async fn reports_the_usage_of_each_request_as_it_ends() {
    let echo = leak(Echo::new("look", ToolOutput::ok("seen")));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "look", json!({}))], "tool_use")],
        )
        .respond(500, vec!["{}"])
        .respond(500, vec!["{}"])
        .respond(500, vec!["{}"]);
    let (agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let mut usages = Usages::default();

    let result = agent
        .run_turn(&mut vec![user("go")], &mut usages, &mut AllowAll)
        .await;

    assert!(result.is_err());
    assert_eq!(
        usages.0.len(),
        1,
        "the request that succeeded is reported even though the turn failed"
    );
    assert_eq!(usages.0[0].output_tokens, 5);
}

#[test]
fn cache_shortfall_is_what_the_previous_request_left_cached_and_this_one_did_not_read() {
    use just_harness::api::Usage;
    let usage = |read: u64, write: u64| Usage {
        input_tokens: 3,
        output_tokens: 9,
        cache_read_input_tokens: Some(read),
        cache_creation_input_tokens: Some(write),
    };

    assert_eq!(usage(1406, 248).cache_shortfall(&usage(1342, 64)), 0);
    assert_eq!(usage(1342, 70).cache_shortfall(&usage(1342, 64)), 64);
    assert_eq!(usage(0, 1600).cache_shortfall(&usage(1342, 64)), 1406);
}

#[test]
fn prompt_tokens_count_uncached_input_and_cache_reads_and_writes() {
    use just_harness::api::Usage;
    let usage = Usage {
        input_tokens: 3,
        output_tokens: 9,
        cache_read_input_tokens: Some(1342),
        cache_creation_input_tokens: Some(64),
    };
    assert_eq!(usage.prompt_tokens(), 1409);
    assert_eq!(usage.history_tokens(), 1418);
    assert_eq!(Usage::default().prompt_tokens(), 0);
}

#[tokio::test]
async fn never_sends_more_than_four_cache_breakpoints() {
    let transport = FakeTransport::new().respond(200, vec![&text_reply("ok", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new());
    agent.system = vec![
        just_harness::api::ContentBlock::text("one").with_cache_control(),
        just_harness::api::ContentBlock::text("two").with_cache_control(),
        just_harness::api::ContentBlock::text("three").with_cache_control(),
        just_harness::api::ContentBlock::text("four"),
    ];
    let mut history = vec![just_harness::api::MessageParam {
        role: just_harness::api::Role::User,
        content: vec![just_harness::api::ContentBlock::text("go")],
    }];

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    let body = sent_body(&transport, 0);
    assert_eq!(body.to_string().matches("cache_control").count(), 4);
    assert_eq!(
        body["system"][3]["cache_control"],
        json!({ "type": "ephemeral" })
    );
    assert!(
        body.get("cache_control").is_none(),
        "no slot is left for automatic caching"
    );
}

#[tokio::test]
async fn a_single_request_has_the_turns_system_tools_and_cache_markers_and_runs_nothing() {
    let echo: &'static Echo = Box::leak(Box::new(Echo::new("lookup", ToolOutput::ok("42"))));
    let transport = FakeTransport::new()
        .respond(200, vec![&text_reply("Hello.", "end_turn")])
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "lookup", json!({}))], "tool_use")],
        );
    let (mut agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    agent.system = vec![api::ContentBlock::text("You are a test.")];
    agent
        .extra
        .insert("output_config".into(), json!({ "effort": "low" }));
    let mut history = vec![user("hi")];
    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();
    let mut messages = history.clone();
    messages.push(user("summarize"));
    let mut usages = 0;
    struct Count<'a>(&'a mut usize);
    impl just_harness::agent::Observer for Count<'_> {
        fn on_usage(&mut self, _usage: &api::Usage) {
            *self.0 += 1;
        }
    }

    let reply = agent
        .send(messages.clone(), &mut Count(&mut usages))
        .await
        .unwrap();

    let (turn, single) = (sent_body(&transport, 0), sent_body(&transport, 1));
    for field in [
        "model",
        "max_tokens",
        "system",
        "tools",
        "cache_control",
        "output_config",
    ] {
        assert_eq!(single[field], turn[field], "{field}");
    }
    assert_eq!(single["messages"], serde_json::to_value(&messages).unwrap());
    assert_eq!(reply.stop_reason, Some(StopReason::ToolUse));
    assert!(echo.calls.lock().unwrap().is_empty());
    assert_eq!(usages, 1);
    assert_eq!(transport.requests().len(), 2);
}

#[tokio::test]
async fn recorded_tool_definitions_are_sent_in_place_of_the_tool_sets() {
    let echo: &'static Echo = Box::leak(Box::new(Echo::new("lookup", ToolOutput::ok("42"))));
    let transport = FakeTransport::new()
        .respond(
            200,
            vec![&tool_reply(&[("toolu_1", "lookup", json!({}))], "tool_use")],
        )
        .respond(200, vec![&text_reply("42.", "end_turn")]);
    let (mut agent, _dir) = agent(&transport, ToolSet::new().with(echo));
    let recorded = api::Tool {
        name: "lookup".into(),
        description: "an older description".into(),
        input_schema: json!({ "type": "object", "properties": {} }),
        cache_control: None,
    };
    agent.tool_definitions = Some(vec![recorded.clone()]);
    let mut history = vec![user("look it up")];

    agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    for n in 0..2 {
        assert_eq!(
            sent_body(&transport, n)["tools"],
            serde_json::to_value(vec![&recorded]).unwrap()
        );
    }
    assert_eq!(echo.calls.lock().unwrap().len(), 1);
}
