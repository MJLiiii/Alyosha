# Scheduled tasks

Alyosha can do things at a set time without being asked at that moment: "remind me Friday at 5," or "every morning at 8, check my email and summarize it." Alyosha's core has no scheduling code. A small MCP server lets the model create tasks, and the chat bridges, which are always running, start each task when it is due and deliver the result to your chat. [Chat channels](Channels.md) · [MCP Client](MCP.md) · [What Alyosha still needs](todo.md) · [Architecture overview](../README.md)

```text
model ── MCP ──► alyosha-schedule ──writes──► ~/.local/share/alyosha/schedule.toml
                                                        │ read every minute
Telegram ◄── alyosha-telegram ◄─────────────────────────┤  telegram tasks
QQ       ◄── alyosha-qq       ◄─────────────────────────┘  qq tasks
              └─ due task → a prompt in the bridge's own session
```

## What a task is

A task has a prompt, a time, and a channel:

- **Prompt.** What Alyosha should do, written as an instruction, such as "Check my inbox and summarize anything important." A reminder is a task too: "Remind the owner to call the bank." That costs one small model call, but there is only one kind of task.
- **Time.** Either one date and time, or a repeating cron expression such as `0 8 * * *` for every day at 8:00. Times use your timezone, Europe/Amsterdam, set by `timezone` in the configuration.
- **Channel.** `telegram` or `qq`, the app whose bridge runs the task and delivers the result.

All tasks live in one file in Alyosha's data folder, `${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/schedule.toml`, which is `~/.local/share/alyosha/schedule.toml` by default. You can read and edit it by hand.

## Creating tasks

The `alyosha-schedule` MCP server offers three tools: `add`, `list`, and `remove`. It is configured like any other MCP server, so the model can schedule from the TUI or from either chat app. [Configuring a server](MCP.md#configuring-a-server)

The model fills in the channel itself. In ACP mode, Alyosha's system instruction names the bridge you are writing through, so a reminder asked for in QQ arrives in QQ. From the TUI, the model asks which app to use unless you said. [What `alyosha acp` does](Channels.md#what-alyosha-acp-does)

## Running tasks

Each bridge checks the file every minute and runs the tasks for its own channel. A due task becomes a prompt in the bridge's ongoing session, marked as scheduled. The chat first shows **⏰ Scheduled:** and the prompt, then the reply as for any message. Because the task runs in the same session as your conversation, you can answer the result directly: "reply to the second email."

- **It shares the queue.** A long task makes your own messages wait, as a long turn does. A message you send during a task waits for it to finish.
- **Overlaps are skipped.** If a repeating task is still running when its next time comes, that run is skipped.
- **Missed one-time tasks run late.** If the bridge was down at the time, the task runs as soon as the bridge is back, marked **late**.
- **Missed repeats are skipped.** A repeating task waits for its next time rather than catching up.
- **One-time tasks disappear after they run.**

## Delivering on QQ

A scheduled task has no message of yours to answer, so the QQ bridge must send on its own, which QQ restricts. It tries a normal message, then one of QQ's "recall" messages, which are limited to four in the 30 days after you last wrote to the bot. If both fail, it keeps the result and sends it first when you next write. [The QQ bridge](Channels.md#the-qq-bridge)

QQ's documentation is unclear on whether a personal bot may send normal messages. If it may not, a daily QQ task reaches you only a few times a month, and the rest wait until you write. The first check with a real QQ bot settles this. If normal messages fail, use Telegram for repeating tasks.

## Seeing what is scheduled

`/tasks` in either chat app lists every task, read directly from the file rather than through the model. The `list` tool gives the model the same list.

## Trust

Scheduled tasks are the first thing the model creates that acts later on its own. A web page that misleads the model could plant a repeating task, such as "every day, send my files to this address." Creating a task needs no approval, because a task can do nothing outward on its own: when it tries to send an email or otherwise reach the real world, it meets the same approval as any other turn, and the request arrives in its chat. [Approvals](Approvals.md) Three things also make planted tasks visible:

- Creating a task is a tool call, shown in the TUI status area and in Telegram's **Working…** message as **MCP: schedule.add**.
- Every run is marked **⏰ Scheduled** in the chat, so a task you did not ask for stands out.
- `/tasks` shows the file's real contents, which the model cannot alter in the reply.

## Deferred

- Running a task in a fresh session of its own instead of the chat's session.
- Reminders sent as plain text without a model call.
- Tasks triggered by events, such as a new email, rather than by time.

[Scheduled tasks implementation guide](../develop/Schedule.md)
