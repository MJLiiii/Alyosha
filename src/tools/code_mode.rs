//! Read-only Code Mode: `execute_code` runs a JavaScript async function in a
//! fresh QuickJS-NG sandbox whose only host operation is `tools.read_file`.
//!
//! Each read goes through shared dispatch with the Code Mode scope, which
//! permits regular workspace files only. Stop interrupts the script even
//! while it computes. See `docs/develop/ToolExecutor.md#read-only-code-mode`.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use rquickjs::{
    AsyncContext, AsyncRuntime, CatchResultExt, CaughtError, Ctx, Exception, Function, Object,
    Promise, Value as JsValue, prelude::Async,
};
use serde_json::{Value, json};
use tokio::sync::Notify;

use super::{
    OutputKind, RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler,
};

pub fn execute_code_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "execute_code".into(),
            description: "Read and filter workspace files with a JavaScript script in a sandbox, returning only what matters. `code` is an async function expression that takes `tools` and resolves to a string. Its only operation is `await tools.read_file({ path, offset?, max_bytes? })`, which resolves to `{ text, next_offset? }` for a regular workspace file (8 KiB per read; pass next_offset as offset to continue) and rejects on an error. One read at a time. No other files, folders, shell, network, or modules. Example:\nasync (tools) => {\n  const { text } = await tools.read_file({ path: \"notes/monday.md\" });\n  return text.split(\"\\n\").filter(l => l.startsWith(\"Decision:\")).join(\"\\n\");\n}".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"code": {"type": "string", "minLength": 1}},
                "required": ["code"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            if context.scope != Scope::Direct {
                return Err(ToolError::failed("execute_code is not available here."));
            }
            let code = arguments["code"].as_str().unwrap_or_default().to_owned();
            // A script can compute without yielding, so it runs on a blocking
            // thread; Stop reaches it through the interrupt handler.
            let runtime = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || runtime.block_on(run_script(code, context)))
                .await
                .map_err(|error| ToolError::failed(format!("The script sandbox failed: {error}")))?
                .map(ToolOutput::text)
        }),
    }
}

/// The host side of one invocation: one read at a time, and closed on exit.
struct Host {
    context: ToolContext,
    in_flight: AtomicUsize,
    closed: AtomicBool,
    idle: Notify,
}

/// One admitted read; releases the host's slot when dropped.
struct Admitted(Arc<Host>);

impl Drop for Admitted {
    fn drop(&mut self) {
        self.0.in_flight.store(0, Ordering::SeqCst);
        self.0.idle.notify_waiters();
    }
}

impl Host {
    /// Admits a read when the script calls `read_file`, before it runs, so an
    /// overlapping call is rejected even if the first read finishes at once.
    fn admit(self: &Arc<Self>) -> Result<Admitted, String> {
        if self.closed.load(Ordering::SeqCst) {
            return Err("the script has ended".into());
        }
        self.in_flight
            .compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst)
            .map_err(|_| "only one read may be in flight; await each read_file".to_owned())?;
        Ok(Admitted(self.clone()))
    }

    async fn read(&self, arguments: Value) -> Result<Value, String> {
        let result = self
            .context
            .executor
            .dispatch(
                "read_file",
                arguments,
                Scope::CodeMode,
                &self.context.cancel,
                None,
            )
            .await;
        match result {
            Ok(output) => {
                let mut value = json!({"text": output.text});
                if let OutputKind::FileChunk { offset, more: true } = output.kind {
                    value["next_offset"] = json!(offset + output.text.len() as u64);
                }
                Ok(value)
            }
            Err(ToolError::Cancelled) => Err("cancelled".into()),
            Err(ToolError::Failed(message)) => Err(message),
        }
    }

    /// Closes the host to new reads and waits for an admitted one.
    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
        loop {
            let idle = self.idle.notified();
            if self.in_flight.load(Ordering::SeqCst) == 0 {
                return;
            }
            idle.await;
        }
    }
}

enum Outcome {
    Done(String),
    Failed(String),
    Cancelled,
}

async fn run_script(code: String, context: ToolContext) -> Result<String, ToolError> {
    let cancel = context.cancel.clone();
    let runtime = AsyncRuntime::new().map_err(|error| {
        ToolError::failed(format!("The script sandbox could not start: {error}"))
    })?;
    {
        let cancel = cancel.clone();
        runtime
            .set_interrupt_handler(Some(Box::new(move || cancel.is_cancelled())))
            .await;
    }
    let js = AsyncContext::full(&runtime).await.map_err(|error| {
        ToolError::failed(format!("The script sandbox could not start: {error}"))
    })?;
    let host = Arc::new(Host {
        context: ToolContext {
            scope: Scope::CodeMode,
            call_id: None,
            ..context
        },
        in_flight: AtomicUsize::new(0),
        closed: AtomicBool::new(false),
        idle: Notify::new(),
    });

    let script_host = host.clone();
    let script_cancel = cancel.clone();
    let outcome = js
        .async_with(async move |ctx| {
            let promise = match start(&ctx, &code, script_host) {
                Ok(promise) => promise,
                Err(message) => return Outcome::Failed(message),
            };
            tokio::select! {
                biased;
                () = script_cancel.cancelled() => Outcome::Cancelled,
                result = promise.into_future::<JsValue>() => match result.catch(&ctx) {
                    Ok(value) => match value.as_string().map(|text| text.to_string()) {
                        Some(Ok(text)) => Outcome::Done(text),
                        _ => Outcome::Failed("The script must resolve to a string.".into()),
                    },
                    Err(error) => Outcome::Failed(describe(error)),
                },
            }
        })
        .await;
    host.close().await;
    drop(js);
    drop(runtime);
    match outcome {
        _ if cancel.is_cancelled() => Err(ToolError::Cancelled),
        Outcome::Done(text) => Ok(text),
        Outcome::Failed(message) => Err(ToolError::failed(message)),
        Outcome::Cancelled => Err(ToolError::Cancelled),
    }
}

/// Evaluates the function expression and calls it with the `tools` object.
fn start<'js>(ctx: &Ctx<'js>, code: &str, host: Arc<Host>) -> Result<Promise<'js>, String> {
    let tools = Object::new(ctx.clone()).map_err(|error| error.to_string())?;
    let read = Function::new(
        ctx.clone(),
        Async(move |ctx: Ctx<'js>, arguments: JsValue<'js>| {
            let arguments = js_to_json(&ctx, arguments);
            let admitted = host.admit();
            async move {
                let admitted =
                    admitted.map_err(|message| Exception::throw_message(&ctx, &message))?;
                let arguments =
                    arguments.map_err(|message| Exception::throw_type(&ctx, &message))?;
                let result = admitted.0.read(arguments).await;
                drop(admitted);
                match result {
                    Ok(value) => json_to_js(&ctx, &value),
                    Err(message) => Err(Exception::throw_message(&ctx, &message)),
                }
            }
        }),
    )
    .map_err(|error| error.to_string())?;
    tools
        .set("read_file", read)
        .map_err(|error| error.to_string())?;
    let function: Function = ctx
        .eval(format!("(\n{code}\n)"))
        .catch(ctx)
        .map_err(|error| {
            format!(
                "The code is not a valid function expression: {}",
                describe(error)
            )
        })?;
    let result: JsValue = function.call((tools,)).catch(ctx).map_err(describe)?;
    match result.into_promise() {
        Some(promise) => Ok(promise),
        None => Err("The code must be an async function, which returns a promise.".into()),
    }
}

fn describe(error: CaughtError<'_>) -> String {
    match error {
        CaughtError::Exception(exception) => {
            let message = exception.message().unwrap_or_default();
            format!("The script failed: {message}")
        }
        CaughtError::Value(value) => format!(
            "The script failed: {}",
            value
                .as_string()
                .and_then(|text| text.to_string().ok())
                .unwrap_or_else(|| format!("{value:?}"))
        ),
        CaughtError::Error(error) => format!("The script failed: {error}"),
    }
}

fn js_to_json<'js>(ctx: &Ctx<'js>, value: JsValue<'js>) -> Result<Value, String> {
    let text = ctx
        .json_stringify(value)
        .map_err(|error| error.to_string())?
        .ok_or("read_file needs an object argument")?
        .to_string()
        .map_err(|error| error.to_string())?;
    serde_json::from_str(&text).map_err(|error| error.to_string())
}

fn json_to_js<'js>(ctx: &Ctx<'js>, value: &Value) -> rquickjs::Result<JsValue<'js>> {
    ctx.json_parse(value.to_string())
}
