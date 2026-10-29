//! Real Messages API exchanges, recorded by `examples/record.rs`, replayed
//! through the client and the agent loop. Each test is set up from the
//! recorded first request and checks that every request it sends equals the
//! recorded one. Expected response values come from `plain_events`, a naive
//! reading of each whole recorded body, not from the decoder that reads the
//! same bytes chunk by chunk.

mod support;

use just_harness::agent::{Agent, AllowAll, Observer, TurnEnd};
use just_harness::api::{BlockKind, Client, CreateMessageRequest, MessageParam, StopReason};
use just_harness::tools::{ToolContext, builtin};
use serde_json::{Map, Value, json};
use support::{FakeTransport, config, replay, sent_body};

/// The `n`th request body recorded for `scenario`.
fn recorded_request(scenario: &str, n: usize) -> Value {
    let path = format!("tests/fixtures/{scenario}/{n:02}.request.json");
    serde_json::from_str(&std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}")))
        .unwrap()
}

fn recorded_count(scenario: &str) -> usize {
    std::fs::read_dir(format!("tests/fixtures/{scenario}"))
        .unwrap()
        .filter(|e| {
            e.as_ref()
                .unwrap()
                .path()
                .extension()
                .is_some_and(|x| x == "body")
        })
        .count()
}

/// The recorded first request as a request to send again.
fn request_from(recorded: &Value) -> CreateMessageRequest {
    let mut request = CreateMessageRequest::new(
        recorded["model"].as_str().unwrap(),
        recorded["max_tokens"].as_u64().unwrap() as u32,
        serde_json::from_value(recorded["messages"].clone()).unwrap(),
    );
    request.system =
        serde_json::from_value(recorded.get("system").cloned().unwrap_or(json!([]))).unwrap();
    request.tools =
        serde_json::from_value(recorded.get("tools").cloned().unwrap_or(json!([]))).unwrap();
    request.extra = extra_fields(recorded);
    request
}

/// Top-level fields other than the ones the request type names.
fn extra_fields(recorded: &Value) -> Map<String, Value> {
    let named = [
        "model",
        "max_tokens",
        "messages",
        "system",
        "tools",
        "stream",
    ];
    recorded
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| !named.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// An agent set up as the scenario's first recorded request says, in `dir`,
/// with the history to start from.
fn agent_from<'t>(
    scenario: &str,
    dir: &std::path::Path,
    transport: &'t FakeTransport,
) -> (Agent<&'t FakeTransport>, Vec<MessageParam>) {
    let first = request_from(&recorded_request(scenario, 1));
    let mut agent = Agent::new(
        Client::with_transport(config(), transport),
        first.model.clone(),
        builtin(),
        ToolContext::new(dir).unwrap(),
    );
    agent.max_tokens = first.max_tokens;
    agent.system = first.system;
    agent.extra = first.extra;
    (agent, first.messages)
}

/// Every request sent equals the one recorded at the same place.
fn assert_requests_match(scenario: &str, transport: &FakeTransport) {
    let n = recorded_count(scenario);
    assert_eq!(transport.requests().len(), n);
    for i in 0..n {
        assert_eq!(
            sent_body(transport, i),
            recorded_request(scenario, i + 1),
            "{scenario} request {}",
            i + 1
        );
    }
}

/// Records the name of every tool the loop runs.
#[derive(Default)]
struct Calls(Vec<String>);

impl Observer for Calls {
    fn on_tool_call(&mut self, name: &str, _input: &Value) {
        self.0.push(name.to_string());
    }
}

/// Each event's JSON in a recorded body, split on blank lines.
fn plain_events(scenario: &str, n: usize) -> Vec<Value> {
    let body = std::fs::read_to_string(format!("tests/fixtures/{scenario}/{n:02}.body")).unwrap();
    body.split("\n\n")
        .filter_map(|event| event.lines().find_map(|l| l.strip_prefix("data: ")))
        .map(|data| serde_json::from_str(data).unwrap())
        .collect()
}

fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

#[tokio::test]
async fn a_recorded_text_reply_assembles_to_what_its_events_say() {
    let events = plain_events("text", 1);
    let transport = replay("text");
    let client = Client::with_transport(config(), &transport);
    let mut seen = 0;

    let message = client
        .send(&request_from(&recorded_request("text", 1)), |_| seen += 1)
        .await
        .unwrap();

    assert_requests_match("text", &transport);

    let text: String = of_type(&events, "content_block_delta")
        .iter()
        .filter_map(|e| e["delta"]["text"].as_str())
        .collect();
    let delta = of_type(&events, "message_delta")[0];
    assert_eq!(message.content.len(), 1);
    assert_eq!(message.content[0].kind(), BlockKind::Text(&text));
    assert_eq!(
        message.id,
        of_type(&events, "message_start")[0]["message"]["id"]
    );
    assert_eq!(message.stop_reason, Some(StopReason::EndTurn));
    assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    assert_eq!(
        json!(message.usage.output_tokens),
        delta["usage"]["output_tokens"]
    );
    assert_eq!(seen, events.iter().filter(|e| e["type"] != "ping").count());
}

#[tokio::test]
async fn the_recorded_tool_loop_runs_its_calls_through_the_real_tools() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("hello.txt"), "hello world\n").unwrap();
    let transport = replay("tool-loop");
    let (agent, mut history) = agent_from("tool-loop", dir.path(), &transport);
    let mut calls = Calls::default();

    let outcome = agent
        .run_turn(&mut history, &mut calls, &mut AllowAll)
        .await
        .unwrap();

    let recorded = recorded_count("tool-loop");
    assert_requests_match("tool-loop", &transport);
    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::EndTurn)));
    assert_eq!(outcome.iterations as usize, recorded);
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hello harness\n"
    );
    let asked: Vec<String> = (1..=recorded)
        .flat_map(|n| plain_events("tool-loop", n))
        .filter(|e| e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use")
        .map(|e| e["content_block"]["name"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(calls.0, asked);
    assert!(
        history
            .iter()
            .flat_map(|m| m.content.iter())
            .all(|b| b.0.get("is_error").is_none())
    );
    assert_eq!(history.len(), 2 * recorded);
}

#[tokio::test]
async fn a_recorded_cut_tool_call_is_kept_and_answered_as_not_run() {
    let events = plain_events("max-tokens", 1);
    let starts = of_type(&events, "content_block_start");
    assert_eq!(starts.len(), 1);
    assert_eq!(starts[0]["content_block"]["type"], "tool_use");
    assert!(
        of_type(&events, "content_block_stop").is_empty(),
        "the recording shows no stop for the cut block"
    );
    let id = starts[0]["content_block"]["id"].as_str().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let transport = replay("max-tokens");
    let (mut agent, mut history) = agent_from("max-tokens", dir.path(), &transport);
    agent.max_iterations = 1;

    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AllowAll)
        .await
        .unwrap();

    assert_requests_match("max-tokens", &transport);
    assert_eq!(outcome.end, TurnEnd::Stopped(Some(StopReason::MaxTokens)));
    assert_eq!(
        outcome.last_message.content[0].kind(),
        BlockKind::ToolUse {
            id,
            name: "edit_file",
            input: &starts[0]["content_block"]["input"]
        }
    );
    let usage = &outcome.last_message.usage;
    assert_eq!(
        json!(usage.input_tokens),
        of_type(&events, "message_start")[0]["message"]["usage"]["input_tokens"]
    );
    assert_eq!(
        json!(usage.output_tokens),
        of_type(&events, "message_delta")[0]["usage"]["output_tokens"]
    );
    assert_ne!(
        of_type(&events, "message_start")[0]["message"]["usage"]["output_tokens"],
        of_type(&events, "message_delta")[0]["usage"]["output_tokens"],
        "the recording should change output_tokens between start and delta"
    );
    assert!(
        std::fs::read_dir(dir.path()).unwrap().next().is_none(),
        "the cut call must not run"
    );
    assert_eq!(history.len(), 3);
    assert_eq!(
        serde_json::to_value(&history[2]).unwrap(),
        json!({ "role": "user", "content": [{
            "type": "tool_result",
            "tool_use_id": id,
            "content": "Not run: the reply stopped at max_tokens.",
            "is_error": true
        }] })
    );
}
