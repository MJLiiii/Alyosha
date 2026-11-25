# Memory

Memory holds facts that outlast a session. Examples are your preferences, facts about you, corrections you have given, and stable facts about your machines and projects. Session history ends with the session. Memory starts in the next one. [Agent State](AgentState.md) · [Architecture overview](../README.md)

V1 stores **curated facts only**. Alyosha does not keep or search past conversations, and it does not learn procedures. Procedures you write yourself are [skills](Skills.md). Every memory is one you or the model wrote on purpose.

## Where memories live

Each memory is a Markdown file in `${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/memory/owner/`. The file name is the memory's name. A short header holds a one-line description and the date of the last update. The body holds the fact.

```markdown
---
description: Thesis repository location and build command
updated: 2026-11-24
---
The thesis lives in ~/thesis. Build it with `latexmk -pdf main.tex`.
```

The store is global. Every workspace sees the same memories. You can read, edit, delete, or back up the files with ordinary tools while Alyosha is stopped. The `owner` directory is the only scope in v1. Group chats can later add sibling scopes, and a session sees only its own scope.

## What the model sees

When a session starts, Alyosha reads the store and builds an **index** with one line per memory: its name, description, and date. Alyosha appends the index to the normal system instruction, along with short guidance on using memory. The instruction stays the same for the whole session, so compaction never removes it and provider prompt caching keeps working. A memory written during the session appears in the index of the next session, which for a chat means after its nightly fresh start. [Chat channels](Channels.md#what-alyosha-acp-does) The model already sees the write in its own history. [Default instruction](LLMClientGateway.md#default-instruction-for-normal-requests) · [Context Manager](ContextManager.md)

The index has a 4 KiB budget. The model reads a memory's full text when it needs it. For example, you ask “build my thesis”. The model sees `thesis-repo — Thesis repository location and build command` in the index and reads that memory before running the build.

## How memories are written

The model writes memories with the `memory` tool during a turn. It writes when you state something lasting, correct it, or tell it to remember something. It does not need your approval. Each write or delete shows in the TUI status area, for example **Remembered: thesis-repo**, so injected instructions cannot change memory unseen. Alyosha sets the update date and keeps the index consistent. [Built-in tool contracts](../reference/BuiltInTools.md#memory)

The guidance tells the model to:

- update an existing memory instead of adding a near-duplicate
- skip facts that the workspace already records or that matter only to the current task
- treat each memory as true as of its date and verify it before relying on it for an action

The tool rejects text that looks like a credential, such as a private key or an API token.

Only direct model calls can use `memory`. Code Mode scripts and shell commands cannot reach the store. [Tool permissions](../reference/ToolPermissions.md#memory-store)

## Deferred

- Recall of past sessions, which requires persistent history.
- Background extraction of memories after a session.
- Per-workspace memory.
- Approval for writes after the turn has read untrusted content.
- A `/memory` command for listing and pruning.

[Memory implementation guide](../develop/Memory.md)
