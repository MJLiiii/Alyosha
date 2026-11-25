# Alyosha architecture

Alyosha is designed as a general-purpose Rust agent for NixOS. You give it a task and a workspace. The model can reply directly or ask to use a tool, inspect the result, and continue. The Rust code for every build step below is written and passes its offline checks under `cargo test`. The checks that need GPROXY, a provider account, the NixOS container, or a real Telegram or QQ bot still have to be run; each guide lists them.

## What v1 must do

V1 is done when these seven tasks work end to end, in the NixOS container, with the GPROXY service and the configuration file as designed:

1. From Telegram, "What came in my inbox today?" returns a short summary.
2. "Reply to Anna that Friday at 7 works" shows Approve and Decline buttons in Telegram, and the email goes out only after Approve.
3. "Find the opening hours of the town hall and remind me tomorrow at 9 to go" searches, reads the page in the browser, schedules a task, and the reminder arrives at 9:00.
4. A daily task, "every morning at 8, summarize my inbox and today's meetings," runs on its own for a week.
5. In the TUI, in a project folder, "Run the tests and fix the failing one" reads and edits files and runs commands in Bubblewrap.
6. In the TUI, "List every decision in my meeting notes" finds the notes with `run_command`, then reads and filters them with one `execute_code` script. Code Mode can only read files, not list folders. [Read-only Code Mode](design/ToolExecutor.md#read-only-code-mode)
7. From QQ, a question gets an answer, and "remind me in ten minutes to stretch" delivers the reminder in QQ, or holds it until the next message if QQ refuses to send first.

## Where to start implementing

Build in this order. Each step builds only on finished ones and ends with a check.

| Step | Build | Done when |
| --- | --- | --- |
| 1 | **One turn in Rust.** Initialize the Cargo workspace at the repository root. Connect the in-memory Agent State, Agent Loop, Tool Registry, and Tool Executor to a scripted model and a fake `read_file` handler. [Turn fixture](develop/AgentLoop.md#verification-with-a-scripted-model-and-fake-tools) | The scripted turn requests two files, one missing, and the second model request carries both results under their original call IDs. The final reply appears in history once. |
| 2 | **Gateway and configuration.** The `alyosha-config` crate, the GPROXY service on the host, `alyosha gateway init` and `gateway login`, the real client against a mock provider and then GPROXY, and the clock line. [Configuration](develop/Configuration.md) · [Gateway setup and checks](develop/LLMClientGateway.md) | One real tool exchange with the configured model works through GPROXY. |
| 3 | **TUI and the remaining built-in tools.** Workspace files, the Bubblewrap shell, QuickJS-NG Code Mode, memory, skills, context compaction and overflow recovery, the queue, Stop, and background jobs. [TUI](develop/TUI.md) · [Jobs](develop/Jobs.md) · [Tool permissions](reference/ToolPermissions.md#verification) · [Executor checks](develop/ToolExecutor.md#verification) · [Context checks](develop/ContextManager.md#verify-the-contracts) | Tasks 5 and 6 work on the host. |
| 4 | **MCP, approvals, and the first plugins.** The MCP Client, the approval step, and `ask_owner`. Then web search, which needs no secrets, then the browser. [MCP](develop/MCP.md) · [Approvals](develop/Approvals.md) · [Web search](develop/WebSearch.md) · [Browser](develop/Browser.md) | In the TUI, "find the opening hours of the town hall" searches and reads the page. |
| 5 | **The container.** Move everything in, including the GPROXY service. Check first that Bubblewrap and Chromium's sandbox start there. [Deployment guide](develop/Deployment.md) | Steps 3 and 4 work again inside the container. |
| 6 | **Email and calendar.** gogcli, signed in on the host. [Email and calendar](develop/EmailCalendar.md) | Tasks 1 and 2 work in the TUI, including the approval. |
| 7 | **ACP mode and the Telegram bridge**, with the shared `alyosha-bridge` crate. [ACP mode](develop/Channels.md) · [Telegram bridge](develop/TelegramBridge.md) | Tasks 1 and 2 work from Telegram. |
| 8 | **Scheduled tasks.** The schedule MCP server and the runner in the bridges. [Scheduled tasks](develop/Schedule.md) | Tasks 3 and 4 work. |
| 9 | **The QQ bridge**, reusing the shared crate and the runner. Settle QQ's open limits with a real bot first. [QQ bridge](develop/QQBridge.md) | Task 7 works. V1 is done. |

The container comes at step 5, not last, because whether Bubblewrap and Chromium's sandbox work inside it is the design's biggest unknown, and because real sign-ins and tokens from step 6 on should only ever be used inside it.

## V1 decisions and open work

| Area | Current decision |
| --- | --- |
| Assistant and model | The normal instruction is “You are Alyosha, a personal assistant.” ChatGPT sign-in and `gpt-6.2-sol` are the defaults. The user changes the account or model in personal configuration. [Gateway design](design/LLMClientGateway.md) |
| Sessions | One turn runs at a time. Messages wait in arrival order. History lives in memory until the session ends. A chat session starts fresh each night at 04:00 when nothing is running. [Agent State](design/AgentState.md) · [Daily fresh session](design/Channels.md#what-alyosha-acp-does) |
| Tools | Nine built-ins cover workspace reads, writes, edits, shell commands, background jobs, read-only JavaScript, memory, skills, and asking the owner. A background job runs a long command while the conversation goes on and starts a turn when it ends. [Background jobs](design/Jobs.md) The shell uses Bubblewrap. JavaScript uses QuickJS-NG through `rquickjs`. [Built-in tools](reference/BuiltInTools.md) · [Code Mode](design/ToolExecutor.md#read-only-code-mode) |
| Interface | The first interface is an inline TUI: the conversation prints into the terminal's scrollback, with a live status line and input box below. [TUI](design/TUI.md) Messaging apps such as Telegram connect through separate bridge programs that drive `alyosha acp` over the Agent Client Protocol, so the core has no code for any app. Telegram and QQ come first, each through a small Rust bridge of Alyosha's own that runs as a service in the container. QQ uses the official bot API, whose reply limits make it a channel for questions and answers. [Chat channels](design/Channels.md) |
| Memory | Curated facts in one global store of Markdown files. The model writes them freely and each write shows in the status area. Past-session recall is deferred. [Memory](design/Memory.md) |
| Skills | Procedures you write, in the Agent Skills layout, in one global directory. The model sees an index and loads a skill with the `skill` tool. Bundled files are read-only at `/skills`. [Skills](design/Skills.md) |
| MCP | Alyosha connects to the stdio MCP servers you configure and registers their tools at session start. Servers run as trusted child processes, and calls need no approval unless the configuration marks the tool `ask`. HTTP servers and OAuth are deferred. [MCP Client](design/MCP.md) |
| Where it runs | The whole agent, with GPROXY and MCP servers, runs as your user in a NixOS container that sees only the workspace folders, `~/work` by default, and Alyosha's own folders. Bubblewrap still wraps each shell command inside it. On the host, `alyosha` runs the container's `alyosha` in the current folder, so it starts from any terminal. The container comes at step 5 of the build order. [Deployment](design/Deployment.md) |
| Scheduled tasks | The model creates timed and repeating tasks through a schedule MCP server. The Telegram and QQ bridges run them in their sessions and deliver the results. [Scheduled tasks](design/Schedule.md) |
| Web search | open-webSearch's MCP server searches engines such as Bing and DuckDuckGo without an account or key. The model opens results in Chromium. SearXNG is the fallback if searches get blocked. [Web search](design/WebSearch.md) |
| Email and calendar | gogcli's MCP server reads Gmail and Google Calendar, tidies the mailbox, writes drafts, and sends email only after you approve. Calendar changes are deferred. [Email and calendar](design/EmailCalendar.md) |
| Approvals | Alyosha acts freely in its own space and asks before reaching the real world. Tools marked `ask` in the configuration, such as sending email, wait for the owner's approval in the TUI or chat. For actions only the model can recognize, such as buying, it calls `ask_owner`. Unanswered requests are declined after 60 minutes. [Approvals](design/Approvals.md) |
| Browser | One MCP server in the container: headless Chromium through Playwright MCP. A skill gives the model the rules for websites. Alyosha never stores passwords: Chromium loads sign-ins you save yourself on the host. [Browser](design/Browser.md) |
| Configuration | One strict `~/.config/alyosha/config.toml`, mode 600, holds every setting and your secrets, and is read by every Alyosha program. Alyosha never writes it. Each message carries a clock line in your configured timezone. The TUI uses the current directory or a path argument as the workspace, in any folder, like Codex or Claude Code. In the container only the shared workspace folders exist. [Configuration](reference/Configuration.md) |

The seven tasks above define v1. The capabilities designed beyond the core are tracked in [What Alyosha still needs](design/todo.md).

## The ten components

Start with the design pages in `docs/design/` to understand each component. The matching guides in `docs/develop/` give its planned Rust contracts, edge cases, and verification steps.

| Component | What it owns | Implementation guide |
| --- | --- | --- |
| [Agent Loop](design/AgentLoop.md) | Starts one turn at a time, requests a model reply, sends each requested call to the executor in order, and records the results. | [Session driver and `run_turn`](develop/AgentLoop.md) |
| [Agent State](design/AgentState.md) | Holds what the session remembers. This includes conversation history, waiting messages, active work, turn outcomes, and the latest context summary. | [Session fields and transitions](develop/AgentState.md) |
| [Tool Registry](design/ToolRegistry.md) | Lists the tools the model can request and associates each name with an input schema and a handler. | [Registration and lookup](develop/ToolRegistry.md) |
| [Context Manager](design/ContextManager.md) | Builds the conversation for the next request from session history. Budgets space for tools and the reply, and summarizes older exchanges when needed. | [Request preparation and compaction](develop/ContextManager.md) |
| [LLM Client / Gateway](design/LLMClientGateway.md) | Sends a prepared request to the selected provider and returns a complete assistant response or an error. Alyosha owns the client and uses GPROXY as the local gateway. | [Model requests and gateway setup](develop/LLMClientGateway.md) |
| [Environment Interface](design/EnvironmentInterface.md) | Gives tool handlers access to the selected workspace, session logs, and sandboxed shell. Enforces permissions at the point of access. | [Tool context and host operations](develop/EnvironmentInterface.md) |
| [Tool Executor](design/ToolExecutor.md) | Handles one call. Looks up the tool, checks the caller's scope and arguments, runs the handler, and returns its output or error. | [Dispatch and cancellation](develop/ToolExecutor.md) |
| [Memory](design/Memory.md) | Keeps curated facts across sessions. Supplies the memory index for the system instruction and stores the `memory` tool's writes. | [Store, index, and tool](develop/Memory.md) |
| [Skills](design/Skills.md) | Reads the procedures you wrote. Supplies the skills index for the system instruction, serves the `skill` tool, and exposes bundled files at `/skills`. | [Store, index, and tool](develop/Skills.md) |
| [MCP Client](design/MCP.md) | Starts the configured MCP servers, registers their tools at session start, and forwards calls to them. | [Servers, registration, and calls](develop/MCP.md) |

## Front ends and shared pieces

These sit around the ten components. The front ends drive the same session driver. The others cross several components.

| Piece | What it owns | Implementation guide |
| --- | --- | --- |
| [TUI](design/TUI.md) | The terminal front end: input, the queue display, the status line, and answering approvals. | [TUI](develop/TUI.md) |
| [ACP mode and bridges](design/Channels.md) | `alyosha acp`, the second front end, and the Telegram and QQ bridges that drive it. | [ACP mode](develop/Channels.md) · [Telegram bridge](develop/TelegramBridge.md) · [QQ bridge](develop/QQBridge.md) |
| [Approvals](design/Approvals.md) | The approval step in the Tool Executor's dispatch, the `ask_owner` tool, and the `Approver` each front end implements. | [Approvals](develop/Approvals.md) |
| [Configuration](reference/Configuration.md) | The one configuration file and the loader every program shares. | [Configuration](develop/Configuration.md) |
| [Scheduled tasks](design/Schedule.md) | The schedule MCP server and the task runner inside the bridges. | [Scheduled tasks](develop/Schedule.md) |
| [Background jobs](design/Jobs.md) | Long shell commands that run outside a turn and start a turn when they end. | [Background jobs](develop/Jobs.md) |
| [Deployment](design/Deployment.md) | The NixOS container around everything. | [Deployment](develop/Deployment.md) |

The browser, web search, and email and calendar are configuration on top of the MCP Client: one MCP server and a skill each. [Browser](design/Browser.md) · [Web search](design/WebSearch.md) · [Email and calendar](design/EmailCalendar.md)

## Follow one turn

```mermaid
flowchart LR
    agentLoop["Agent Loop"] -->|updates history| state["Agent State"]
    agentLoop -->|reads definitions| registry["Tool Registry"]
    agentLoop -->|requests context| context["Context Manager"]
    agentLoop -->|requests a reply| client["LLM Client / Gateway"]
    context -->|requests a summary| client
    agentLoop -->|dispatches a call| executor["Tool Executor"]
    executor -->|looks up a handler| registry
    executor -->|handler requests file or shell access| env["Environment Interface"]
    agentLoop -->|loads the index at session start| memory["Memory"]
    executor -->|memory tool| memory
    agentLoop -->|loads the index at session start| skills["Skills"]
    executor -->|skill tool| skills
    mcp["MCP Client"] -->|registers server tools at session start| registry
    executor -->|MCP tool call| mcp
    executor -->|approval or ask_owner question| frontend["Front end: TUI or ACP"]
```

Suppose you ask, “Which operating system does Alyosha target?”

1. The **Agent Loop** starts the turn and appends your question to **Agent State**. It gets the available tool definitions from the **Tool Registry**.
2. The **Context Manager** prepares the conversation to send and budgets for those definitions. The loop passes the request to the **LLM Client / Gateway**. Suppose the model replies with a call to `read_file` for `docs/develop/EnvironmentInterface.md`.
3. The loop records that complete assistant response and passes the call to the **Tool Executor**. The executor looks up `read_file`, checks its arguments and scope, and invokes its handler. The handler opens the file through the **Environment Interface**, which checks that the target is inside the workspace.
4. The executor returns the text. The loop records a tool result under the original call ID. On the next model request, the model sees its call and the result together. A reply of “NixOS” with no further calls ends the turn and appears once in history.

## Where Alyosha runs

The components above run together in one process tree inside a NixOS container, with the workspaces and Alyosha's configuration and data mounted from the host. None of them depends on the container. The browser is configuration on top of this, not a component: one MCP server and a skill. [Deployment design](design/Deployment.md) · [Container setup and checks](develop/Deployment.md) · [Browser](design/Browser.md)

## Shared references

- To change what `read_file` accepts or returns, see the [built-in tool contracts](reference/BuiltInTools.md).
- To change which files it may open, see the [tool permission policy](reference/ToolPermissions.md).
- To add or change a setting, see the [configuration reference](reference/Configuration.md).
- To find out what happened, or to log something new, see [logging](reference/Logging.md).
