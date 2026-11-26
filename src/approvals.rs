//! Approvals: the approval step in shared dispatch, the `ask_owner` tool, and
//! the `Approver` each front end implements.
//!
//! Tools the configuration marks `ask` wait for the owner before they run.
//! Alyosha formats the arguments itself, so a misled model cannot describe a
//! harmful call in harmless words. See `docs/develop/Approvals.md`.

use std::{sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    BoxFuture,
    tools::{RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler},
};

/// A request shown to the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerRequest {
    /// The model's tool call ID.
    pub call_id: String,
    /// "Approve? google.gmail_send_message", or the `ask_owner` question.
    pub title: String,
    /// The formatted arguments; empty for `ask_owner`.
    pub body: String,
    /// The last option is always the negative one.
    pub options: Vec<String>,
}

/// How the owner answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerAnswer {
    Chosen(usize),
    NoAnswer,
    Undeliverable,
    Cancelled,
}

/// The front end's way of asking the owner.
pub trait Approver: Send + Sync {
    /// Waits for an answer. A front end that cannot ask returns
    /// `Undeliverable`.
    fn ask(&self, request: OwnerRequest, cancel: CancellationToken) -> BoxFuture<'_, OwnerAnswer>;
}

/// Asks through the session's approver, with the configured time limit.
pub(crate) async fn ask(
    context: &ToolContext,
    request: OwnerRequest,
    timeout: Duration,
) -> OwnerAnswer {
    let Some(approver) = context.session.approver.clone() else {
        return OwnerAnswer::Undeliverable;
    };
    // The question and the arguments stay out of the log.
    tracing::info!(
        timeout_s = timeout.as_secs(),
        "waiting for the owner's answer"
    );
    let started = std::time::Instant::now();
    let answer = tokio::select! {
        biased;
        () = context.cancel.cancelled() => OwnerAnswer::Cancelled,
        answer = approver.ask(request, context.cancel.clone()) => answer,
        () = tokio::time::sleep(timeout) => OwnerAnswer::NoAnswer,
    };
    let outcome = match answer {
        OwnerAnswer::Chosen(_) => "answered",
        OwnerAnswer::NoAnswer => "expired",
        OwnerAnswer::Undeliverable => "undeliverable",
        OwnerAnswer::Cancelled => "cancelled",
    };
    tracing::info!(
        outcome,
        duration_ms = started.elapsed().as_millis() as u64,
        "the owner's answer"
    );
    answer
}

/// The session's approval time limit, carried by its approver setup.
pub fn approval_timeout(context: &ToolContext) -> Duration {
    context
        .session
        .approval_timeout
        .unwrap_or(Duration::from_secs(60 * 60))
}

fn minutes(timeout: Duration) -> u64 {
    timeout.as_secs().div_ceil(60).max(1)
}

/// The approval step of shared dispatch, after validation and before
/// admission.
pub(crate) async fn approve(
    definition: &ToolDefinition,
    arguments: &Value,
    context: &ToolContext,
) -> Result<(), ToolError> {
    let shown = display_name(&definition.name);
    context.notice(format!("Waiting for approval: {shown}"));
    let request = OwnerRequest {
        call_id: context
            .call_id
            .as_ref()
            .map(|id| id.0.clone())
            .unwrap_or_default(),
        title: format!("Approve? {shown}"),
        body: format_body(&definition.input_schema, arguments),
        options: vec!["Approve".into(), "Decline".into()],
    };
    let timeout = approval_timeout(context);
    match ask(context, request, timeout).await {
        OwnerAnswer::Chosen(0) => Ok(()),
        OwnerAnswer::Chosen(_) => Err(ToolError::failed(
            "The owner declined this call. It did not run.",
        )),
        OwnerAnswer::NoAnswer => Err(ToolError::failed(format!(
            "The owner did not answer within {} minutes. The call did not run.",
            minutes(timeout)
        ))),
        OwnerAnswer::Undeliverable => Err(ToolError::failed(
            "The approval request could not be delivered. The call did not run.",
        )),
        OwnerAnswer::Cancelled => Err(ToolError::Cancelled),
    }
}

/// `mcp__google__gmail_send_message` is shown as `google.gmail_send_message`.
pub fn display_name(name: &str) -> String {
    match name
        .strip_prefix("mcp__")
        .and_then(|rest| rest.split_once("__"))
    {
        Some((server, tool)) => format!("{server}.{tool}"),
        None => name.to_owned(),
    }
}

/// One `name: value` line per argument, in the schema's property order.
/// Strings are written in full with their line breaks; a multi-line value
/// starts on the line after its name. Other values are compact JSON.
pub fn format_body(schema: &Value, arguments: &Value) -> String {
    let Some(arguments) = arguments.as_object() else {
        return arguments.to_string();
    };
    let mut names: Vec<&String> = schema["properties"]
        .as_object()
        .map(|properties| {
            properties
                .keys()
                .filter(|name| arguments.contains_key(*name))
                .collect()
        })
        .unwrap_or_default();
    for name in arguments.keys() {
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let mut lines = Vec::new();
    for name in names {
        match &arguments[name] {
            Value::String(text) if text.contains('\n') => lines.push(format!("{name}:\n{text}")),
            Value::String(text) => lines.push(format!("{name}: {text}")),
            other => lines.push(format!("{name}: {other}")),
        }
    }
    lines.join("\n")
}

/// The built-in `ask_owner` tool.
pub fn ask_owner_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "ask_owner".into(),
            description: "Ask the owner a question and wait for the answer. Use it before an action that reaches the real world, such as buying, posting, or sending something to another person, and whenever a decision is the owner's to make. Without choices the question is yes or no; with choices the owner picks one or none of them.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "minLength": 1, "maxLength": 1000},
                    "choices": {
                        "type": "array",
                        "items": {"type": "string", "minLength": 1, "maxLength": 100},
                        "minItems": 1,
                        "maxItems": 6,
                        "uniqueItems": true
                    }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            if context.scope != Scope::Direct {
                return Err(ToolError::failed("ask_owner is for direct calls only."));
            }
            let question = arguments["question"].as_str().unwrap_or_default().to_owned();
            let choices: Vec<String> = arguments["choices"]
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default();
            let options = if choices.is_empty() {
                vec!["Yes".to_owned(), "No".to_owned()]
            } else {
                let mut options = choices.clone();
                options.push("None of these".into());
                options
            };
            context.notice("Asking the owner");
            let request = OwnerRequest {
                call_id: context.call_id.as_ref().map(|id| id.0.clone()).unwrap_or_default(),
                title: question,
                body: String::new(),
                options: options.clone(),
            };
            let timeout = approval_timeout(&context);
            let text = match ask(&context, request, timeout).await {
                OwnerAnswer::Chosen(index) if choices.is_empty() => {
                    format!("The owner answered: {}", options.get(index).map_or("No", String::as_str))
                }
                OwnerAnswer::Chosen(index) if index < choices.len() => {
                    format!("The owner chose: {}", choices[index])
                }
                OwnerAnswer::Chosen(_) => "The owner chose none of the choices.".into(),
                OwnerAnswer::NoAnswer => {
                    format!("The owner did not answer within {} minutes.", minutes(timeout))
                }
                OwnerAnswer::Undeliverable => {
                    return Err(ToolError::failed("The question could not be delivered."));
                }
                OwnerAnswer::Cancelled => return Err(ToolError::Cancelled),
            };
            Ok(ToolOutput::text(text))
        }),
    }
}

/// An approver that always gives the same answer, for tests and for front
/// ends that cannot ask.
pub struct FixedApprover(pub OwnerAnswer);

impl Approver for FixedApprover {
    fn ask(
        &self,
        _request: OwnerRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, OwnerAnswer> {
        let answer = self.0;
        Box::pin(async move { answer })
    }
}

/// Called with a request and true when asking starts, false when it ends.
pub type OnAsk = Arc<dyn Fn(&OwnerRequest, bool) + Send + Sync>;

/// Wraps an approver so the session can note when the owner is being asked.
pub struct ObservedApprover {
    pub inner: Arc<dyn Approver>,
    pub on_ask: OnAsk,
}

impl Approver for ObservedApprover {
    fn ask(&self, request: OwnerRequest, cancel: CancellationToken) -> BoxFuture<'_, OwnerAnswer> {
        Box::pin(async move {
            (self.on_ask)(&request, true);
            struct Done<'a>(&'a ObservedApprover, OwnerRequest);
            impl Drop for Done<'_> {
                fn drop(&mut self) {
                    (self.0.on_ask)(&self.1, false);
                }
            }
            let _done = Done(self, request.clone());
            self.inner.ask(request, cancel).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_body_follows_the_schema_order_and_keeps_line_breaks() {
        let schema = json!({"type": "object", "properties": {
            "to": {"type": "string"}, "subject": {"type": "string"}, "body": {"type": "string"}, "cc": {"type": "array"}
        }});
        let arguments = json!({
            "body": "Hi Anna,\nFriday at 7 works for me.",
            "cc": ["b@example.com"],
            "to": "anna@example.com",
            "subject": "Re: Dinner on Friday",
        });
        assert_eq!(
            format_body(&schema, &arguments),
            "to: anna@example.com\nsubject: Re: Dinner on Friday\nbody:\nHi Anna,\nFriday at 7 works for me.\ncc: [\"b@example.com\"]"
        );
    }

    #[test]
    fn mcp_names_display_as_server_dot_tool() {
        assert_eq!(
            display_name("mcp__google__gmail_send_message"),
            "google.gmail_send_message"
        );
        assert_eq!(display_name("read_file"), "read_file");
    }
}
