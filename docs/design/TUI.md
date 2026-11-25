# TUI

The TUI is how you use Alyosha at a terminal. It runs one session in the folder you start it in. The conversation prints into the terminal's normal scrollback, and only a small area at the bottom is live: a status line and the input box. You keep your terminal's own scrolling, search, and copy and paste, and the conversation stays on screen after you quit. [Agent Loop](AgentLoop.md) · [Chat channels](Channels.md) · [Architecture overview](../README.md)

```text
alyosha · ~/Projects/thesis · gpt-6.2-sol
! MCP: 1 problem at startup (search: failed to start), details in ~/.local/share/alyosha/logs/alyosha-tui.2026-11-25.log

› what's on my calendar tomorrow?
  · MCP: google.calendar_events
Tomorrow you have two meetings: 10:00 thesis review with Prof. Li, and …

› email Anna that 7 works
  · Waiting for approval: google.gmail_send_message
  Approve? google.gmail_send_message
  to: anna@example.com
  subject: Re: Dinner on Friday
  body:
  Hi Anna, Friday at 7 works for me…
──────────────────────────────────────────────────────────────
 Waiting for you · 1 queued                          Esc stop
 [1] Approve   [2] Decline    (y / n)
```

## What appears where

**The scrollback** keeps the record:

- Your messages, after `›`. A message sent while a turn runs is marked **(queued)** until its turn starts.
- Alyosha's replies, printed whole when the turn ends, because the model client returns complete replies. Replies are plain text with long lines wrapped. Markdown stays as written.
- A finished background job's message, as **· Job j1 finished**, before the turn it starts.
- One dim line, after `·`, for each tool call and notice: **MCP: server.tool**, **Remembered: name**, **Compacting context…**, **Waiting for approval: server.tool**, **Asking the owner**. Tool calls stay visible after the turn, not only while they run. [Memory](Memory.md#what-the-model-sees) · [MCP Client](MCP.md#one-call)
- Startup warnings, after `!`, such as MCP servers that failed to start or skills that could not be loaded. Each names the log file that holds the details. [Logging](../reference/Logging.md)
- The full text of every approval request and `ask_owner` question. [Approvals](Approvals.md)
- How a turn ended when it did not end with a reply: **Stopped**, or **Turn failed** with the reason.

**The status line** shows what is happening now: **Idle**, **Thinking 12s**, **Running google.gmail_search**, **Waiting for you**, or **Compacting**, then the number of queued messages, the number of running background jobs, and a key hint. [Background jobs](Jobs.md)

## Keys and commands

| Key or command | What it does |
| --- | --- |
| Enter | Send the message. During a turn it joins the queue. |
| Alt+Enter or Ctrl+J | Start a new line in the message. |
| Up and Down | Recall earlier messages from this session. |
| Esc | Stop the running turn. With an approval open, decline it and stop. |
| Ctrl+C | Stop the running turn. When idle, clear the input. Twice on an empty input quits. |
| Ctrl+D or `/quit` | Quit. A running turn is stopped first. The conversation ends with the session, as designed. With background jobs running, the first press warns and a second one stops them and quits. |
| `/compact` | Summarize older conversation. When busy, the TUI says to try again when idle. [Manual compaction](ContextManager.md#compact-command) |
| `/job <command>` | Start the command as a background job, even during a turn. [Background jobs](Jobs.md#starting-a-job) |
| `/help` | List the keys and commands. |

## Approvals and questions

When Alyosha needs your approval or asks you a question, the request prints to the scrollback and the input box becomes its options. Answer with `y` or `n` for two options, or with an option's number. Esc declines and stops the turn. Messages you sent before stay in the queue. An unanswered request expires after the configured time like anywhere else. [Approvals](Approvals.md#rules)

## Starting

- **Workspace.** `alyosha` uses the current folder, and `alyosha <path>` uses another. It must be a folder under `~/Projects`, otherwise Alyosha stops with an error saying so. [Container limits](../reference/ToolPermissions.md#container-limits)
- **Header.** One line names the workspace and the model, followed by any startup warnings.
- **Signing in.** The TUI never signs in to the model provider. If the provider is not signed in, the first turn fails with a message telling you to run `alyosha gateway login`. [Gateway login](../develop/LLMClientGateway.md#run-gproxy-as-a-service)

## Deferred

- Rendering Markdown, such as bold text, headings, and highlighted code.
- Editing or removing a queued message.
- `/new`. Quit and start again instead.
- Choosing the model with a command. It is set in the configuration.
- Running a skill directly, such as `/blog-release`. [Skills deferred work](Skills.md#deferred)

[TUI implementation guide](../develop/TUI.md)
