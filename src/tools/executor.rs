//! Tool Executor: runs one tool call and returns its outcome.
//!
//! The Agent Loop calls it for each tool the model requests, and build step 3
//! adds Code Mode's reads as a second caller. Both go through the same
//! dispatch checks. See `docs/ToolExecutor.md#dispatch-one-call`.

use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::{
    Scope, SessionServices, ToolContext, ToolDefinition, ToolError, ToolOutput, ToolRegistry,
};
use crate::agent_state::CallId;

/// The tool error for a call that needs approval when no front end can ask.
pub const APPROVAL_UNDELIVERABLE: &str =
    "The approval request could not be delivered. The call did not run.";

/// Dispatches calls to the handlers of one registry, with one session's
/// services.
#[derive(Clone)]
pub struct ToolExecutor {
    registry: Arc<ToolRegistry>,
    session: Arc<SessionServices>,
}

impl std::fmt::Debug for ToolExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolExecutor")
            .field("registry", &self.registry)
            .finish_non_exhaustive()
    }
}

impl ToolExecutor {
    pub fn new(registry: Arc<ToolRegistry>, session: Arc<SessionServices>) -> Self {
        Self { registry, session }
    }

    pub fn registry(&self) -> &Arc<ToolRegistry> {
        &self.registry
    }

    pub fn session(&self) -> &Arc<SessionServices> {
        &self.session
    }

    /// Runs one call: look up the exact name, check the caller's scope,
    /// validate the arguments, admit the call unless Stop came first, and await
    /// the handler. A rejected call returns a tool error before any handler
    /// runs.
    pub async fn dispatch(
        &self,
        name: &str,
        arguments: Value,
        scope: Scope,
        cancel: &CancellationToken,
        call_id: Option<CallId>,
    ) -> Result<ToolOutput, ToolError> {
        let Some(tool) = self.registry.get(name) else {
            return Err(ToolError::failed(format!("Unknown tool: {name}")));
        };
        if !scope.allows(name) {
            return Err(ToolError::failed(format!(
                "The {name} tool is not available here."
            )));
        }
        validate_arguments(&tool.definition, &arguments)?;
        let context = ToolContext {
            scope,
            cancel: cancel.clone(),
            session: self.session.clone(),
            executor: self.clone(),
            call_id,
        };
        if tool.definition.requires_approval {
            // Code Mode never gets here: its scope allows only read_file.
            crate::approvals::approve(&tool.definition, &arguments, &context).await?;
        }
        // Check Stop and admit the call in one step: nothing is awaited between
        // the check and the handler's start.
        if cancel.is_cancelled() {
            return Err(ToolError::Cancelled);
        }
        (tool.handler)(arguments, context).await
    }
}

/// Checks a call's arguments against the tool's input schema.
fn validate_arguments(definition: &ToolDefinition, arguments: &Value) -> Result<(), ToolError> {
    // The registry accepted this schema, so it compiles.
    let validator = jsonschema::validator_for(&definition.input_schema).map_err(|error| {
        ToolError::failed(format!(
            "The {} tool has an unusable input schema: {error}",
            definition.name
        ))
    })?;
    let problems: Vec<String> = validator
        .iter_errors(arguments)
        .map(|error| {
            let path = error.instance_path().to_string();
            if path.is_empty() {
                error.to_string()
            } else {
                format!("{path}: {error}")
            }
        })
        .collect();
    if problems.is_empty() {
        Ok(())
    } else {
        Err(ToolError::failed(format!(
            "Invalid arguments for {}: {}",
            definition.name,
            problems.join("; ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    use serde_json::json;

    use super::*;
    use crate::tools::{RegisteredTool, handler};

    /// A registry with a counting `read_file` and a counting `memory` tool.
    fn registry(runs: &Arc<AtomicUsize>, memory_requires_approval: bool) -> Arc<ToolRegistry> {
        let mut registry = ToolRegistry::new();
        for (name, requires_approval) in
            [("read_file", false), ("memory", memory_requires_approval)]
        {
            let runs = runs.clone();
            registry
                .register(RegisteredTool {
                    definition: ToolDefinition {
                        name: name.to_owned(),
                        description: String::new(),
                        input_schema: json!({
                            "type": "object",
                            "properties": {
                                "path": {"type": "string"},
                                "offset": {"type": "integer", "minimum": 0}
                            },
                            "required": ["path"]
                        }),
                        requires_approval,
                    },
                    handler: handler(move |args: Value, context: ToolContext| {
                        let runs = runs.clone();
                        async move {
                            runs.fetch_add(1, Ordering::SeqCst);
                            Ok(ToolOutput::text(format!(
                                "{} in {:?}",
                                args["path"].as_str().unwrap(),
                                context.scope
                            )))
                        }
                    }),
                })
                .unwrap();
        }
        Arc::new(registry)
    }

    #[tokio::test]
    async fn only_a_valid_call_in_scope_runs() {
        let runs = Arc::new(AtomicUsize::new(0));
        let executor = ToolExecutor::new(registry(&runs, false), Default::default());
        let cancel = CancellationToken::new();

        let output = executor
            .dispatch(
                "read_file",
                json!({"path": "a.txt"}),
                Scope::Direct,
                &cancel,
                None,
            )
            .await;
        assert_eq!(output, Ok(ToolOutput::text("a.txt in Direct")));
        assert_eq!(runs.load(Ordering::SeqCst), 1);

        let invalid = executor
            .dispatch(
                "read_file",
                json!({"offset": -1}),
                Scope::Direct,
                &cancel,
                None,
            )
            .await
            .unwrap_err();
        let ToolError::Failed(message) = invalid else {
            panic!("expected a tool error")
        };
        assert!(
            message.starts_with("Invalid arguments for read_file:"),
            "{message}"
        );
        assert!(
            message.contains("\"path\" is a required property"),
            "{message}"
        );
        assert!(
            message.contains("/offset: -1 is less than the minimum of 0"),
            "{message}"
        );

        let unknown = executor
            .dispatch(
                "frobnicate",
                json!({"path": "a.txt"}),
                Scope::Direct,
                &cancel,
                None,
            )
            .await;
        assert_eq!(unknown, Err(ToolError::failed("Unknown tool: frobnicate")));

        let outside = executor
            .dispatch(
                "memory",
                json!({"path": "a.txt"}),
                Scope::CodeMode,
                &cancel,
                None,
            )
            .await;
        assert_eq!(
            outside,
            Err(ToolError::failed("The memory tool is not available here."))
        );

        assert_eq!(runs.load(Ordering::SeqCst), 1, "only the valid call ran");
    }

    #[tokio::test]
    async fn code_mode_may_read_files() {
        let runs = Arc::new(AtomicUsize::new(0));
        let executor = ToolExecutor::new(registry(&runs, false), Default::default());
        let output = executor
            .dispatch(
                "read_file",
                json!({"path": "notes.md"}),
                Scope::CodeMode,
                &CancellationToken::new(),
                None,
            )
            .await;
        assert_eq!(output, Ok(ToolOutput::text("notes.md in CodeMode")));
    }

    #[tokio::test]
    async fn stop_before_admission_runs_nothing() {
        let runs = Arc::new(AtomicUsize::new(0));
        let executor = ToolExecutor::new(registry(&runs, false), Default::default());
        let cancel = CancellationToken::new();
        cancel.cancel();
        let output = executor
            .dispatch(
                "read_file",
                json!({"path": "a.txt"}),
                Scope::Direct,
                &cancel,
                None,
            )
            .await;
        assert_eq!(output, Err(ToolError::Cancelled));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_call_needing_approval_does_not_run_without_a_front_end() {
        let runs = Arc::new(AtomicUsize::new(0));
        let executor = ToolExecutor::new(registry(&runs, true), Default::default());
        let output = executor
            .dispatch(
                "memory",
                json!({"path": "a.txt"}),
                Scope::Direct,
                &CancellationToken::new(),
                None,
            )
            .await;
        assert_eq!(output, Err(ToolError::failed(APPROVAL_UNDELIVERABLE)));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
}
