//! Records real Messages API exchanges as test fixtures under
//! `tests/fixtures/<scenario>/`: for each request, `NN.request.json` holds its
//! JSON body (headers, and so the key, are not stored), `NN.body` the response
//! body bytes as received and `NN.json` the response status, kept headers and
//! chunk sizes. `shell` and `edit_file` calls are asked about at the terminal.
//! Fixtures are written only when every scenario finished with 200 responses.
//! Needs `ANTHROPIC_API_KEY`; spends API credit. Run: `cargo run --example record`.

use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt;
use just_harness::agent::{Agent, Approval, ApprovalRequest, Approver, TurnOutcome};
use just_harness::api::{
    ApiError, Client, ClientConfig, ContentBlock, CreateMessageRequest, HttpRequest, HttpResponse,
    MessageParam, ReqwestTransport, Role, Transport,
};
use just_harness::tools::{ToolContext, builtin};
use serde_json::{Value, json};

const MODEL: &str = "claude-haiku-5-5";
/// Response headers written to fixtures.
const KEPT_HEADERS: [&str; 2] = ["content-type", "request-id"];
const SYSTEM: &str = "You are a coding agent working in a folder. Use the tools to search, \
                      read and edit files there and to run commands. Paths are relative to \
                      that folder.";

struct Recorded {
    request: Value,
    status: u16,
    headers: Vec<(String, String)>,
    chunks: Vec<Vec<u8>>,
}

type Log = Arc<Mutex<Vec<Recorded>>>;

/// The real transport, with every request body and response copied.
struct Recording {
    inner: ReqwestTransport,
    log: Log,
}

impl Transport for Recording {
    async fn send(&self, request: HttpRequest) -> Result<HttpResponse, ApiError> {
        let body: Value = serde_json::from_slice(&request.body)
            .map_err(|e| ApiError::Protocol(format!("request body: {e}")))?;
        let response = self.inner.send(request).await?;
        let index = {
            let mut log = self.log.lock().unwrap();
            log.push(Recorded {
                request: body,
                status: response.status,
                headers: response
                    .headers
                    .iter()
                    .filter(|(k, _)| KEPT_HEADERS.iter().any(|h| k.eq_ignore_ascii_case(h)))
                    .cloned()
                    .collect(),
                chunks: Vec::new(),
            });
            log.len() - 1
        };
        let log = self.log.clone();
        let body = response.body.map(move |chunk| {
            if let Ok(bytes) = &chunk {
                log.lock().unwrap()[index].chunks.push(bytes.to_vec());
            }
            chunk
        });
        Ok(HttpResponse {
            status: response.status,
            headers: response.headers,
            body: Box::pin(body),
        })
    }
}

/// Asks y/n on the terminal, showing the tool's preview.
struct AskAtTerminal;

impl Approver for AskAtTerminal {
    async fn approve(&mut self, request: &ApprovalRequest<'_>) -> Approval {
        println!("[{}]\n{}", request.name, request.preview);
        loop {
            print!("allow? [y/n]: ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            if std::io::stdin().read_line(&mut answer).unwrap_or(0) == 0 {
                return Approval::Stop;
            }
            match answer.trim() {
                "y" => return Approval::Allow,
                "n" => return Approval::Deny(None),
                _ => {}
            }
        }
    }
}

fn client(config: &ClientConfig, log: &Log) -> Result<Client<Recording>, ApiError> {
    let transport = Recording {
        inner: ReqwestTransport::new()?,
        log: log.clone(),
    };
    Ok(Client::with_transport(config.clone(), transport))
}

fn user(text: &str) -> Vec<MessageParam> {
    vec![MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }]
}

fn report(scenario: &str, outcome: &TurnOutcome, history: &[MessageParam]) {
    println!(
        "{scenario}: {:?} after {} requests, last usage {:?}",
        outcome.end, outcome.iterations, outcome.last_message.usage
    );
    for message in history {
        let kinds: Vec<&str> = message
            .content
            .iter()
            .map(ContentBlock::block_type)
            .collect();
        println!("  {:?} {kinds:?}", message.role);
    }
}

fn write_fixtures(scenario: &str, log: &Log) -> std::io::Result<()> {
    let dir = Path::new("tests/fixtures").join(scenario);
    if dir.exists() {
        std::fs::remove_dir_all(&dir)?;
    }
    std::fs::create_dir_all(&dir)?;
    for (i, recorded) in log.lock().unwrap().iter().enumerate() {
        let n = i + 1;
        let pretty = |v: &Value| serde_json::to_string_pretty(v).map(|s| s + "\n");
        std::fs::write(
            dir.join(format!("{n:02}.request.json")),
            pretty(&recorded.request)?,
        )?;
        std::fs::write(dir.join(format!("{n:02}.body")), recorded.chunks.concat())?;
        let meta = json!({
            "status": recorded.status,
            "headers": recorded.headers,
            "chunks": recorded.chunks.iter().map(Vec::len).collect::<Vec<_>>(),
        });
        std::fs::write(dir.join(format!("{n:02}.json")), pretty(&meta)?)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = ClientConfig::from_env()?;
    let mut recorded: Vec<(&str, Log)> = Vec::new();

    // `text`: one reply, no tools.
    let log = Log::default();
    let request = CreateMessageRequest::new(MODEL, 1024, user("Reply with exactly one word: pong"));
    let reply = client(&config, &log)?.send(&request, |_| {}).await?;
    println!("text: {:?} {:?}", reply.stop_reason, reply.usage);
    recorded.push(("text", log));

    // `tool-loop`: read, edit and check a file through the agent.
    let log = Log::default();
    let dir = tempfile::tempdir()?;
    std::fs::write(dir.path().join("hello.txt"), "hello world\n")?;
    let mut agent = Agent::new(
        client(&config, &log)?,
        MODEL,
        builtin(),
        ToolContext::new(dir.path())?,
    );
    agent.system = vec![ContentBlock::text(SYSTEM)];
    agent
        .extra
        .insert("output_config".into(), json!({ "effort": "high" }));
    let mut history = user(
        "Read hello.txt, change \"world\" to \"harness\" in it, then run cat hello.txt to confirm.",
    );
    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AskAtTerminal)
        .await?;
    report("tool-loop", &outcome, &history);
    println!(
        "  hello.txt = {:?}",
        std::fs::read_to_string(dir.path().join("hello.txt"))?
    );
    recorded.push(("tool-loop", log));

    // `max-tokens`: a tool call cut off by a small `max_tokens`.
    let log = Log::default();
    let dir = tempfile::tempdir()?;
    let mut agent = Agent::new(
        client(&config, &log)?,
        MODEL,
        builtin(),
        ToolContext::new(dir.path())?,
    );
    agent.system = vec![ContentBlock::text(SYSTEM)];
    agent
        .extra
        .insert("output_config".into(), json!({ "effort": "low" }));
    agent.max_tokens = 200;
    agent.max_iterations = 1;
    let mut history = user(
        "Use edit_file to create numbers.txt containing the numbers 1 to 300, one per line. Do not explain, just call the tool.",
    );
    let outcome = agent
        .run_turn(&mut history, &mut (), &mut AskAtTerminal)
        .await?;
    report("max-tokens", &outcome, &history);
    recorded.push(("max-tokens", log));

    for (scenario, log) in &recorded {
        let statuses: Vec<u16> = log.lock().unwrap().iter().map(|r| r.status).collect();
        if statuses.iter().any(|&s| s != 200) {
            return Err(format!(
                "{scenario}: statuses {statuses:?}; nothing written, record again"
            )
            .into());
        }
    }
    for (scenario, log) in &recorded {
        write_fixtures(scenario, log)?;
    }
    println!("fixtures written");
    Ok(())
}
