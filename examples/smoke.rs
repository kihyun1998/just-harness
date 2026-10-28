//! Three live calls against the Messages API: a text reply, a tool call at
//! high effort, and the tool result sent back with the assistant turn unchanged.
//! Needs `ANTHROPIC_API_KEY`. Run: `cargo run --example smoke`.

use just_harness::api::{
    BlockKind, Client, ClientConfig, ContentBlock, CreateMessageRequest, Message, MessageParam,
    Role, Tool,
};
use serde_json::json;

const MODEL: &str = "claude-haiku-5-5";

fn user(text: &str) -> Vec<MessageParam> {
    vec![MessageParam {
        role: Role::User,
        content: vec![ContentBlock::text(text)],
    }]
}

fn report(label: &str, message: &Message, events: usize) {
    println!("== {label}");
    println!(
        "id={} model={} stop_reason={:?} events={events}",
        message.id, message.model, message.stop_reason
    );
    println!("usage={:?}", message.usage);
    for block in &message.content {
        match block.kind() {
            BlockKind::Text(t) => println!("  text: {t}"),
            BlockKind::Thinking(t) => println!(
                "  thinking: {} chars, signature {} chars",
                t.len(),
                block
                    .0
                    .get("signature")
                    .and_then(|s| s.as_str())
                    .map_or(0, str::len)
            ),
            BlockKind::ToolUse { id, name, input } => println!("  tool_use: {name} {input} ({id})"),
            BlockKind::Other(kind) => println!("  other block: {kind}"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = Client::new(ClientConfig::from_env()?)?;

    let mut events = 0;
    let text = client
        .send(
            &CreateMessageRequest::new(MODEL, 2048, user("Reply with exactly one word: pong")),
            |_| events += 1,
        )
        .await?;
    report("text", &text, events);

    let mut request =
        CreateMessageRequest::new(MODEL, 2048, user("What is in src/main.rs? Use the tool."));
    request.tools.push(Tool {
        name: "read_file".into(),
        description: "Read a file from the project and return its contents.".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Path relative to the project root" } },
            "required": ["path"]
        }),
        cache_control: None,
    });
    request
        .extra
        .insert("output_config".into(), json!({ "effort": "high" }));
    let mut events = 0;
    let tool = client.send(&request, |_| events += 1).await?;
    report("tool", &tool, events);

    let Some(BlockKind::ToolUse { id, .. }) = tool
        .content
        .iter()
        .map(ContentBlock::kind)
        .find(|k| matches!(k, BlockKind::ToolUse { .. }))
    else {
        println!("no tool_use block; skipping the tool result call");
        return Ok(());
    };
    request.messages.push(tool.to_param());
    request.messages.push(MessageParam {
        role: Role::User,
        content: vec![ContentBlock::tool_result(
            id,
            "fn main() { println!(\"hello\"); }",
            false,
        )],
    });
    let mut events = 0;
    let answer = client.send(&request, |_| events += 1).await?;
    report("tool result", &answer, events);
    Ok(())
}
