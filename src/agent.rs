//! The agent loop: send the conversation, run the tools the model asks for,
//! append the results, and repeat until the model stops asking.

use serde_json::Value;

use crate::api::{
    ApiError, BlockKind, Client, ContentBlock, CreateMessageRequest, Message, MessageParam, Role,
    StopReason, Transport, Usage,
};
use crate::tools::{ToolContext, ToolOutput, ToolSet};

pub const DEFAULT_MAX_ITERATIONS: u32 = 50;

/// What the loop reports while it runs.
pub trait Observer {
    /// One stream event's JSON, as it arrives.
    fn on_event(&mut self, _event: &Value) {}
    fn on_tool_call(&mut self, _name: &str, _input: &Value) {}
    fn on_tool_result(&mut self, _name: &str, _output: &ToolOutput) {}
    /// A message `run_turn` appended to the history, after it was appended.
    fn on_message(&mut self, _message: &MessageParam) {}
    /// The usage of a request that just ended.
    fn on_usage(&mut self, _usage: &Usage) {}
}

impl Observer for () {}

/// The answer to a tool call that needs approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Allow,
    /// Do not run it; the optional reason goes to the model.
    Deny(Option<String>),
    /// Do not run it or the reply's remaining calls, and end the turn after
    /// the model's next reply.
    Stop,
}

/// A tool call waiting for approval, with what to show and what an
/// allow-for-the-session answer would cover.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest<'a> {
    pub name: &'a str,
    pub input: &'a Value,
    /// The tool's text for the user: the command, or the edit.
    pub preview: String,
    /// Equal keys are the same call for a session-wide allow.
    pub key: String,
}

/// Decides tool calls whose tool needs approval.
pub trait Approver {
    fn approve(&mut self, request: &ApprovalRequest<'_>) -> impl Future<Output = Approval>;
}

/// Allows every call.
pub struct AllowAll;

impl Approver for AllowAll {
    async fn approve(&mut self, _request: &ApprovalRequest<'_>) -> Approval {
        Approval::Allow
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TurnEnd {
    /// The model's last message ended with this stop reason.
    Stopped(Option<StopReason>),
    /// The iteration cap was reached with tool calls still being made.
    IterationLimit,
    /// The user answered [`Approval::Stop`].
    StoppedByUser,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TurnOutcome {
    pub end: TurnEnd,
    /// Requests sent in this turn.
    pub iterations: u32,
    /// Token usage summed over the turn's requests.
    pub usage: Usage,
    pub last_message: Message,
}

pub struct Agent<T: Transport> {
    pub client: Client<T>,
    pub model: String,
    pub max_tokens: u32,
    pub system: Vec<ContentBlock>,
    /// Further top-level request fields, sent with every request.
    pub extra: serde_json::Map<String, Value>,
    pub tools: ToolSet,
    pub context: ToolContext,
    /// Requests one turn may send.
    pub max_iterations: u32,
    /// Whether requests ask for prompt caching.
    pub prompt_cache: bool,
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
            extra: serde_json::Map::new(),
            tools,
            context,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            prompt_cache: true,
        }
    }

    /// Runs one user turn on `history`, which must end with the user's
    /// message. Each assistant message and tool-result message is appended;
    /// nothing already in `history` is changed.
    pub async fn run_turn(
        &self,
        history: &mut Vec<MessageParam>,
        observer: &mut impl Observer,
        approver: &mut impl Approver,
    ) -> Result<TurnOutcome, ApiError> {
        let mut iterations = 0;
        let mut usage = Usage::default();
        let mut user_stopped = false;
        loop {
            let message = self.send(history.clone(), observer).await?;
            iterations += 1;
            usage.add(&message.usage);
            append(history, observer, message.to_param());

            if user_stopped {
                if let Some(not_run) = not_run_results(&message, USER_STOPPED) {
                    append(history, observer, not_run);
                }
                return Ok(outcome(
                    TurnEnd::StoppedByUser,
                    iterations,
                    usage.clone(),
                    message,
                ));
            }
            let stopped = TurnEnd::Stopped(message.stop_reason.clone());
            match next_step(message.stop_reason.as_ref()) {
                Step::Stop => {
                    let why = format!("the reply stopped at {}", stop_reason_name(&message));
                    if let Some(not_run) = not_run_results(&message, &why) {
                        append(history, observer, not_run);
                    }
                    return Ok(outcome(stopped, iterations, usage.clone(), message));
                }
                Step::Resume => {}
                Step::RunTools => match self.run_tools(&message, observer, approver).await {
                    Some((results, stop)) => {
                        append(history, observer, results);
                        user_stopped = stop;
                    }
                    None => return Ok(outcome(stopped, iterations, usage.clone(), message)),
                },
            }
            if !user_stopped && iterations >= self.max_iterations {
                return Ok(outcome(
                    TurnEnd::IterationLimit,
                    iterations,
                    usage.clone(),
                    message,
                ));
            }
        }
    }

    /// Sends `messages` as one request with this agent's model, system,
    /// tools, extra fields and cache markers, and reports its events and
    /// usage. Tool calls in the reply are not run.
    pub async fn send(
        &self,
        messages: Vec<MessageParam>,
        observer: &mut impl Observer,
    ) -> Result<Message, ApiError> {
        let mut request = CreateMessageRequest::new(&self.model, self.max_tokens, messages);
        request.system = self.system.clone();
        request.tools = self.tools.definitions();
        request.extra = self.extra.clone();
        if self.prompt_cache {
            add_cache_breakpoints(&mut request);
        }
        let message = self.client.send(&request, |e| observer.on_event(e)).await?;
        observer.on_usage(&message.usage);
        Ok(message)
    }

    /// The user message carrying one result per `tool_use` block, in order,
    /// and whether the user answered [`Approval::Stop`]; `None` when the
    /// message has no tool calls. Calls after a stop are not run or asked.
    async fn run_tools(
        &self,
        message: &Message,
        observer: &mut impl Observer,
        approver: &mut impl Approver,
    ) -> Option<(MessageParam, bool)> {
        let mut results = Vec::new();
        let mut stopped = false;
        for block in &message.content {
            let BlockKind::ToolUse { id, name, input } = block.kind() else {
                continue;
            };
            observer.on_tool_call(name, input);
            let output = if stopped {
                ToolOutput::error(not_run(USER_STOPPED))
            } else if self.tools.needs_approval(name) {
                let (preview, key) = self.tools.approval_view(name, input, &self.context);
                let request = ApprovalRequest {
                    name,
                    input,
                    preview,
                    key,
                };
                match approver.approve(&request).await {
                    Approval::Allow => self.tools.call(name, input, &self.context).await,
                    Approval::Deny(None) => ToolOutput::error("The user denied this call."),
                    Approval::Deny(Some(reason)) => {
                        ToolOutput::error(format!("The user denied this call. Reason: {reason}"))
                    }
                    Approval::Stop => {
                        stopped = true;
                        ToolOutput::error(not_run(USER_STOPPED))
                    }
                }
            } else {
                self.tools.call(name, input, &self.context).await
            };
            observer.on_tool_result(name, &output);
            results.push(ContentBlock::tool_result(
                id,
                output.content,
                output.is_error,
            ));
        }
        (!results.is_empty()).then_some((
            MessageParam {
                role: Role::User,
                content: results,
            },
            stopped,
        ))
    }
}

const USER_STOPPED: &str = "the user stopped the turn";

fn not_run(why: &str) -> String {
    format!("Not run: {why}.")
}

fn stop_reason_name(message: &Message) -> String {
    message
        .stop_reason
        .as_ref()
        .and_then(|r| serde_json::to_value(r).ok())
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_else(|| "no stop reason".into())
}

/// The most `cache_control` breakpoints one request may carry.
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// Marks the last system block and asks for automatic caching, within the
/// breakpoint limit and counting the markers the request already has.
fn add_cache_breakpoints(request: &mut CreateMessageRequest) {
    let marked = |block: &ContentBlock| block.0.get("cache_control").is_some();
    let mut count = request
        .tools
        .iter()
        .filter(|t| t.cache_control.is_some())
        .count()
        + request.system.iter().filter(|b| marked(b)).count()
        + request
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter(|b| marked(b))
            .count();
    if count < MAX_CACHE_BREAKPOINTS
        && let Some(last) = request.system.pop()
    {
        if !marked(&last) {
            count += 1;
        }
        request.system.push(if marked(&last) {
            last
        } else {
            last.with_cache_control()
        });
    }
    if count < MAX_CACHE_BREAKPOINTS {
        request.cache_control = Some(serde_json::json!({ "type": "ephemeral" }));
    }
}

/// One `is_error` result per tool call in `message`, saying it was not run
/// because of `why`. `None` when the message has no tool calls.
fn not_run_results(message: &Message, why: &str) -> Option<MessageParam> {
    let results: Vec<ContentBlock> = message
        .content
        .iter()
        .filter_map(|block| match block.kind() {
            BlockKind::ToolUse { id, .. } => {
                Some(ContentBlock::tool_result(id, not_run(why), true))
            }
            _ => None,
        })
        .collect();
    (!results.is_empty()).then_some(MessageParam {
        role: Role::User,
        content: results,
    })
}

/// Appends `message` to `history` and reports it.
fn append(history: &mut Vec<MessageParam>, observer: &mut impl Observer, message: MessageParam) {
    history.push(message);
    if let Some(last) = history.last() {
        observer.on_message(last);
    }
}

fn outcome(end: TurnEnd, iterations: u32, usage: Usage, last_message: Message) -> TurnOutcome {
    TurnOutcome {
        end,
        iterations,
        usage,
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
