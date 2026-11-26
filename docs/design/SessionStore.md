# Durable sessions

A session's conversation survives a restart. Alyosha writes every history entry to a PostgreSQL database as it happens. When a bridge or its `alyosha acp` restarts, after a crash, a configuration change, or a container reboot, the conversation picks up where it stopped. A session ends only when you start a new one. There is no nightly fresh start. [Agent State](AgentState.md) · [Chat channels](Channels.md) · [Architecture overview](../README.md)

## What is saved

| Saved | Lost on restart |
| --- | --- |
| Which session each chat is using | Messages waiting in the queue |
| Every history entry: your messages, the model's replies and tool calls with provider replay data, and tool results | A turn in progress, which ends as stopped |
| Each compaction's summary and where verbatim history resumes | Open approvals and `ask_owner` questions |
| | Background jobs, MCP servers and their state, such as browser tabs, and the dev shell |
| | Saved full outputs of shortened results (`log_path`) |

Memory, skills, `schedule.toml`, and `config.toml` stay files you edit by hand. [Memory](Memory.md) · [Scheduled tasks](Schedule.md)

## How it works

**An append-only log.** History is already append-only, and only `run_turn` appends to it. Each entry is written to the database at that point, before it joins the in-memory history. If the write fails, the turn fails, so what the model saw and what is saved never disagree. A crash loses nothing that was committed, including tool calls that already ran in an unfinished turn. Saving a snapshot of the whole session at the end of each turn would be simpler, but a crash mid-turn would forget tool calls that had already acted on the world.

**A key per conversation.** Each place a conversation happens has a stable key, and its current session is the newest session with that key:

| Where | Key |
| --- | --- |
| Your private chat | `owner:telegram`, `owner:qq` |
| A trusted group | `group:telegram:<chat ID>`, `group:qq:<group_openid>` |
| A guest group | `guest:telegram:<chat ID>`, `guest:qq:<group_openid>` |

A bridge that starts asks to load its key's current session and starts a new one only if there is none. `/new` starts a new session under the same key. The old one stays in the database. A guest session stopped after an hour of quiet resumes on the next mention instead of starting blank.

**One process per session.** A process holds a database lock on the session it runs. A second process that tries to open the same session, such as a second bridge started by hand while the service runs, stops with **This session is open in another Alyosha process.** The lock goes away with the process, so a crash never leaves it held.

**Resuming.** Alyosha loads the latest summary and the entries after it. Older entries stay in the database but not in memory. If the last reply asked for tool calls that have no results, Alyosha records each as an error: *Alyosha restarted before this call finished. It may or may not have run; check before repeating it.* The first reply after a resume starts with *Resumed after a restart.*, adding *The turn in progress was stopped, and messages sent while Alyosha was down may need resending.* when a turn was interrupted.

## Without the nightly fresh start

The 04:00 fresh start was how a chat session got rid of what it should not carry for weeks. Each of those now has its own answer:

| Before, renewed nightly | Now |
| --- | --- |
| History | Compaction bounds what the model sees. After a restart, only the entries since the last summary are loaded. [Context Manager](ContextManager.md) |
| Memory and skills indexes | Rebuilt at each compaction and each resume. A compaction already changes the start of the conversation, so the instruction changing at the same moment costs little prompt caching. [Memory](Memory.md#what-the-model-sees) |
| Saved full outputs | Removed at each compaction for results the summary replaced, and at restart. Reading a removed `log_path` gives a tool error saying the output is gone. |
| Configuration, MCP servers, and the dev shell | Picked up when the bridge restarts, which no longer loses the conversation: `systemctl restart alyosha-telegram`. |

## The database

PostgreSQL runs as a service inside the NixOS container. It listens only on a Unix socket, not on any network address, and accepts your user by peer authentication, so there is no password to keep. Its data lives in the container's own state, and a daily dump goes to `~/.local/share/alyosha/backups/`, which is mounted from the host. [Deployment](Deployment.md)

The model's shell runs in Bubblewrap, which does not mount the socket, so the model cannot read or change its history with `psql`. MCP servers run outside the sandbox as you and could connect, like everything else they can reach. [Tool permissions](../reference/ToolPermissions.md#container-limits)

The database holds what was said, which the logs never do. [Logging](../reference/Logging.md)

If the database cannot be reached, `alyosha acp` stops with an error instead of running without saving.

## Deferred

- Saving the message queue, so messages sent during a crash are answered after it.
- Searching or recalling past sessions. [Memory deferred work](Memory.md#deferred)
- An archive of chat messages as the apps show them, including group messages that do not mention Alyosha.
- Deleting old sessions. V1 keeps them all.
- Resuming background jobs, open approvals, or saved full outputs after a restart.
- Listing or choosing among a key's earlier sessions.

[Durable sessions implementation guide](../develop/SessionStore.md)
