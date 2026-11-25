# Approvals

Alyosha acts freely inside its own space: workspace files, shell commands, memory, scheduled tasks, drafts, and tidying your mailbox. It asks you first when an action reaches the real world, such as other people, money, or public posts, or when a decision should be yours. [Architecture overview](../README.md) · [What Alyosha still needs](todo.md)

## Two kinds of asking

Alyosha can recognize some real-world actions by the tool alone. Others look like ordinary calls, and only the model knows what they will do. So there are two kinds of asking, and both reach you the same way.

| | Enforced approval | Model-requested question |
| --- | --- | --- |
| For | Tools whose purpose is to act outward, such as sending an email | Actions only the model can recognize, such as a click on "Buy", and decisions that are yours, such as choosing between two flights |
| Who decides to ask | Your configuration marks the tool `ask` | The model calls the built-in `ask_owner` tool |
| Can the model skip it | No. The Tool Executor stops before running the tool. | Yes. Skills tell it when to ask, but nothing enforces that. |
| What you see | The exact call: the tool and its arguments, formatted by Alyosha | The model's question and its choices |

The enforced kind is a step in the Tool Executor's dispatch, after the arguments are checked and before the tool runs. Because Alyosha formats the arguments itself, a misled model cannot describe a harmful call in harmless words. [Shared dispatch](ToolExecutor.md#shared-dispatch)

```toml
[mcp.servers.google]
ask = ["gmail_send_message"]
```

For example, before an email goes out you see:

```text
Approve? google.gmail_send_message
to: anna@example.com
subject: Re: Dinner on Friday
body:
Hi Anna, Friday at 7 works for me…
[Approve] [Decline]
```

`ask_owner` takes a question and up to six choices, or none for a yes-or-no question. Its result tells the model what you chose. The browser and email skills tell the model to use it before buying, posting, or submitting anything to another person or service, and whenever a choice is yours to make. [Built-in tool contracts](../reference/BuiltInTools.md#ask_owner)

## How you are asked

Each session asks through its own front end:

| Session | How you answer |
| --- | --- |
| TUI | The request prints in the conversation, and the input box shows the options. [TUI](TUI.md#approvals-and-questions) |
| Telegram | Buttons under the request |
| QQ | A reply of `/yes` or `/no`, or the number of a choice, because buttons need extra permission from QQ |

In ACP mode both kinds use ACP's `session/request_permission`, so each bridge handles one kind of request. [Chat channels](Channels.md)

## Rules

- **One call at a time.** An approval covers that call only. There is no "always allow."
- **Declining is an answer.** The tool does not run, and the model receives a tool error saying you declined. It continues the turn and can explain or try something else.
- **No answer means no.** After 60 minutes without an answer, the call does not run and the model is told you did not answer. The limit is configurable. Meanwhile the turn waits. In a chat, messages you send while a request is open wait in the queue behind it. In the TUI, the input box waits for your answer first, and messages you queued before stay queued. [TUI](TUI.md#approvals-and-questions)
- **Stop declines.** Stop or `/cancel` while a request is open declines it and stops the turn.
- **Undelivered means no.** If a bridge cannot deliver the request, for example when QQ refuses to send, the call does not run.
- **Scheduled tasks ask too.** A task that wants to send an email at 7:00 asks in its chat. If you are asleep, the call is declined after 60 minutes. Creating or removing a task needs no approval, because a task can do nothing outward without asking. [Scheduled tasks](Schedule.md#trust)

## What approvals do not cover

- **Deleting files with the shell.** `run_command` runs every command through the same tool, so Alyosha cannot tell `rm` from `ls`. Workspaces are the real `~/Projects` folders, so git history is the safety net there.
- **Browser actions the model does not ask about.** A purchase is the same `browser_click` as any other click. If the model is misled into not asking, nothing stops it. The status area and Telegram's **Working…** message still name every click.

## Deferred

- "Always allow" choices, remembered per tool or per recipient.
- Approval rules that depend on the arguments, such as allowing email to some addresses without asking.
- Approval for built-in tools.

[Approvals implementation guide](../develop/Approvals.md)
