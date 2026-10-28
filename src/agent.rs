//! The agent loop: send the conversation, run the tools the model asks for,
//! append the results, and repeat until the model stops asking.

use serde_json::Value;

use crate::api::{
    ApiError, BlockKind, Client, ContentBlock, CreateMessageRequest, Message, MessageParam, Role,
    StopReason, Transport,
};
use crate::tools::{ToolContext, ToolOutput, ToolSet};

pub const DEFAULT_MAX_ITERATIONS: u32 = 50;

/// What the loop reports while it runs.
pub trait Observer {
    /// One stream event's JSON, as it arrives.
    fn on_event(&mut self, _event: &Value) {}
    fn on_tool_call(&mut self, _name: &str, _input: &Value) {}
    fn on_tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
}

impl Observer for () {}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    /// The model's last message ended with this stop reason.
    Stopped(Option<StopReason>),
    /// The iteration cap was reached with tool calls still being made.
    IterationLimit,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub end: TurnEnd,
    /// Requests sent in this turn.
    pub iterations: u32,
    pub last_message: Message,
}

pub struct Agent<T: Transport> {
    pub client: Client<T>,
    pub model: String,
    pub max_tokens: u32,
    pub system: Vec<ContentBlock>,
    pub tools: ToolSet,
    pub context: ToolContext,
    /// Requests one turn may send.
    pub max_iterations: u32,
}

impl<T: Transport> Agent<T> {
    pub fn new(
        client: Client<T>,
        model: impl Into<String>,
        tools: ToolSet,
        context: ToolContext,
    ) -> Self {
        Self {
            client,
            model: model.into(),
            max_tokens: 64_000,
            system: Vec::new(),
            tools,
            context,
            max_iterations: DEFAULT_MAX_ITERATIONS,
        }
    }

    /// Runs one user turn on `history`, which must end with the user's
    /// message. Each assistant message and tool-result message is appended;
    /// nothing already in `history` is changed.
    pub async fn run_turn(
        &self,
        history: &mut Vec<MessageParam>,
        observer: &mut impl Observer,
    ) -> Result<TurnOutcome, ApiError> {
        let mut iterations = 0;
        loop {
            let mut request =
                CreateMessageRequest::new(&self.model, self.max_tokens, history.clone());
            request.system = self.system.clone();
            request.tools = self.tools.definitions();
            let message = self.client.send(&request, |e| observer.on_event(e)).await?;
            iterations += 1;
            history.push(message.to_param());

            let stopped = TurnEnd::Stopped(message.stop_reason.clone());
            match next_step(message.stop_reason.as_ref()) {
                Step::Stop => {
                    if let Some(not_run) = not_run_results(&message) {
                        history.push(not_run);
                    }
                    return Ok(outcome(stopped, iterations, message));
                }
                Step::Resume => {}
                Step::RunTools => match self.run_tools(&message, observer).await {
                    Some(results) => history.push(results),
                    None => return Ok(outcome(stopped, iterations, message)),
                },
            }
            if iterations >= self.max_iterations {
                return Ok(outcome(TurnEnd::IterationLimit, iterations, message));
            }
        }
    }

    /// The user message carrying one result per `tool_use` block, in order,
    /// or `None` when the message has no tool calls.
    async fn run_tools(
        &self,
        message: &Message,
        observer: &mut impl Observer,
    ) -> Option<MessageParam> {
        let mut results = Vec::new();
        for block in &message.content {
            if let BlockKind::ToolUse { id, name, input } = block.kind() {
                observer.on_tool_call(name, input);
                let output = self.tools.call(name, input, &self.context).await;
                observer.on_tool_result(name, &output);
                results.push(ContentBlock::tool_result(
                    id,
                    output.content,
                    output.is_error,
                ));
            }
        }
        (!results.is_empty()).then_some(MessageParam {
            role: Role::User,
            content: results,
        })
    }
}

/// For a reply that stopped with tool calls in it: one `is_error` result per
/// call saying it was not run. `None` when the reply has no tool calls.
fn not_run_results(message: &Message) -> Option<MessageParam> {
    let reason = message
        .stop_reason
        .as_ref()
        .and_then(|r| serde_json::to_value(r).ok())
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "no stop reason".into());
    let results: Vec<ContentBlock> = message
        .content
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::ToolUse { id, .. } => Some(ContentBlock::tool_result(
                id,
                format!("Not run: the reply stopped at {reason}."),
                true,
            )),
            _ => None,
        })
        .collect();
    (!results.is_empty()).then_some(MessageParam {
        role: Role::User,
        content: results,
    })
}

fn outcome(end: TurnEnd, iterations: u32, last_message: Message) -> TurnOutcome {
    TurnOutcome {
        end,
        iterations,
        last_message,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Run the turn's tool calls and send their results.
    RunTools,
    /// Send the conversation back unchanged so the server continues the turn.
    Resume,
    /// The turn is over; its tool calls are answered as not run.
    Stop,
}

fn next_step(stop_reason: Option<&StopReason>) -> Step {
    match stop_reason {
        Some(StopReason::ToolUse) => Step::RunTools,
        Some(StopReason::PauseTurn) => Step::Resume,
        _ => Step::Stop,
    }
}
