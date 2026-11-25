# Implement approvals

Add one dispatch step to the Tool Executor, one built-in tool, and one front-end operation that the TUI and the ACP mode each implement. The bridges show the requests. [Approvals design](../design/Approvals.md) · [Tool Executor](ToolExecutor.md) · [Architecture overview](../README.md)

## Configuration

```toml
[approvals]
timeout_minutes = 60            # default 60, at least 1

[mcp.servers.google]
ask = ["gmail_send_message"]    # default empty
```

`McpServerConfig` gains `ask: Vec<String>`, holding original tool names on that server. At registration, mark each listed tool's definition `requires_approval = true`. A name in `ask` that `tools` leaves out is a configuration error, so Alyosha refuses to start, because a misspelled entry would otherwise turn off an approval without notice. [Loading](Configuration.md#loading) A name the server does not offer can only be found at startup, so it gives a startup warning. That tool is not registered, so nothing runs without approval. No built-in tool requires approval. [MCP configuration](MCP.md#v1-configuration) · [Registry interface](ToolRegistry.md#v1-interface)

## The front-end operation

```text
Approver
    ask(request: OwnerRequest, cancel: CancellationToken) -> OwnerAnswer

OwnerRequest {
    call_id: String           # the model's tool call ID
    title: String             # "Approve? google.gmail_send_message", or the ask_owner question
    body: String              # the formatted arguments; empty for ask_owner
    options: Vec<String>      # ["Approve", "Decline"], ["Yes", "No"], or the choices plus "None of these"
}

OwnerAnswer = Chosen(index) | NoAnswer | Undeliverable | Cancelled
```

The session driver gives `run_turn` the front end's `Approver`, and the executor reaches it through `ToolContext`. The last option of every request is the negative one: Decline, No, or None of these. `ask` waits for the first of: an answer, the configured timeout, which gives `NoAnswer`, or cancellation, which gives `Cancelled`. A front end that cannot ask returns `Undeliverable`. While `ask` waits, the active turn's phase is `WaitingForOwner` with the call's ID. [Active turn fields](AgentState.md#active-turn-fields)

Format `body` from the validated arguments in their schema's property order, one `name: value` line each. Write string values in full and keep their line breaks, putting a multi-line value on the lines after `name:`. Write other values as compact JSON. Never shorten the body. Bridges split long messages.

## The dispatch step

In shared dispatch, after validating the arguments and before admitting the call: [Dispatch one call](ToolExecutor.md#dispatch-one-call)

1. If the definition does not require approval, continue as before.
2. Emit the status notice **Waiting for approval: server.tool**.
3. Call `ask` with `options = ["Approve", "Decline"]`, then act on the answer:

| Answer | Outcome |
| --- | --- |
| `Chosen(0)` | Continue to admission and run the tool. The tool's own deadline starts now. |
| `Chosen(1)` | Tool error: "The owner declined this call. It did not run." |
| `NoAnswer` | Tool error: "The owner did not answer within N minutes. The call did not run." |
| `Undeliverable` | Tool error: "The approval request could not be delivered. The call did not run." |
| `Cancelled` | Cancellation, so the turn ends as `Stopped`. |

Code Mode never reaches this step, because it cannot call MCP tools. [Read-only Code Mode](ToolExecutor.md#read-only-code-mode)

## The `ask_owner` tool

Register `ask_owner` with the built-ins. It is available to direct model calls only. [Built-in tool contracts](../reference/BuiltInTools.md#ask_owner)

| Input | Type | Rule |
| --- | --- | --- |
| `question` | string | 1 to 1000 characters |
| `choices` | array of strings, optional | 1 to 6 items, each 1 to 100 characters, no duplicates |

Without `choices`, the options are Yes and No. With them, the options are the choices followed by None of these. Emit the status notice **Asking the owner**, call `ask` with an empty body, and return text the model can act on:

| Answer | Result |
| --- | --- |
| A choice | `The owner chose: <choice>` |
| Yes or No | `The owner answered: Yes` or `The owner answered: No` |
| None of these | `The owner chose none of the choices.` |
| `NoAnswer` | `The owner did not answer within N minutes.` |
| `Undeliverable` | Tool error: "The question could not be delivered." |
| `Cancelled` | Cancellation. |

## The TUI

Show the title, the body, and numbered options in the TUI, and read one key: the option's number, or `y` and `n` for two options. [TUI approvals](TUI.md#approvals)

## The ACP mode

Send `session/request_permission` with:

- `toolCall`: `toolCallId` set to the call ID, `title` set to the request title, and `content` holding the body as one text block when it is not empty.
- `options`: one per option, with `optionId` set to its index as a string, `name` set to its text, and `kind` set to `reject_once` for the last option and `allow_once` for the others.

Map the reply: `selected` with an index gives `Chosen(index)`, and `cancelled` gives `Undeliverable` unless the session's prompt was cancelled, which gives `Cancelled`. A bridge sends `session/cancel` before answering open requests with `cancelled`, as ACP requires, so the turn's token is already signalled and `ask` returns `Cancelled`. On timeout, stop waiting, send a `tool_call_update` for that call ID with `status: "failed"`, and ignore any later reply, so the bridge can mark the request expired. [ACP mode](Channels.md) · [Permission requests](https://agentclientprotocol.com/protocol/tool-calls)

## Verification

These run under `cargo test` with a scripted model, a fake MCP server, and a scripted `Approver`.

- Configure `ask` with a name the server does not offer. Check for a startup warning. Configure one that `tools` leaves out. Check that loading the configuration fails.
- Call a tool marked `ask` and approve it. Check that the server receives the call only after the answer and that the request body shows every argument in full, including line breaks.
- Decline, let it time out with a 1-minute limit, and make the front end return `Undeliverable`. Check that the server never receives the call and that each case returns its own tool error to the model.
- Press Stop while a request is open. Check that the turn ends `Stopped` and the server never receives the call.
- Call a tool not marked `ask`. Check that `ask` is never called.
- Call `ask_owner` with no choices, with three choices, and with seven. Check the options offered, each result text, and the schema error for seven.
- In ACP mode, check that `session/request_permission` carries the call ID, the body, and options whose last one is `reject_once`. Time it out and check for a `failed` update and that a late reply is ignored. Send `session/cancel` and then answer the open request with `cancelled`. Check that the turn ends `Stopped`, not with an undelivered error.

With the bridges and real apps:

- In Telegram, ask Alyosha to email yourself. Check that the buttons show the full message, that Approve sends it, and that a second try with Decline does not.
- In QQ, ask a question that makes the model call `ask_owner` with choices. Answer with a number and check that the model receives your choice.
- Schedule a task that sends an email in two minutes. Do not answer. Check that the email is not sent and the request is marked expired.

[Architecture overview](../README.md)
