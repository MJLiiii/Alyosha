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

The store is global. Every workspace sees the same memories. You can read, edit, delete, or back up the files with ordinary tools while Alyosha is stopped. The `owner` directory holds what Alyosha knows about you. Your sessions, in the TUI and your private chats, see only it. Guest sessions in group chats never see it; they use the notes kept for each person in the sibling `users` directory. [People in group chats](#people-in-group-chats)

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

## People in group chats

In group chats anyone can talk to Alyosha, and it keeps one Markdown file of notes for each person, in `${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/memory/users/`. The file is named by the platform and the person's user ID, such as `telegram-123456789.md` or `qq-<member_openid>.md`, because display names change and can be copied. [Group chats](Channels.md#group-chats)

```markdown
---
name: Bob
updated: 2026-11-25
---
Prefers answers in Chinese. Studies physics in Delft.
```

- **One file, not an index.** A person's notes are short, at most 2 KiB, so there is no index. Each message from that person reaches the model with their whole notes file in front of it. A message from someone without notes says so.
- **Written only for the speaker.** The model rewrites the notes with the `user_memory` tool. Alyosha decides whose file it writes, from the sender the bridge reports for the message being answered, so nobody can make Alyosha change another person's notes. The header's name is the person's display name at the time of the last write. [Built-in tool contracts](../reference/BuiltInTools.md#user_memory)
- **What goes in.** The guest instruction tells the model to note what helps it answer that person later: the language they prefer, what they work on, what they asked it to remember. It never stores secrets, and the tool rejects text that looks like a credential, as the `memory` tool does.
- **Not private from the group.** Notes come from what a person said in a group, and they enter that group's conversation when the person writes. Tell people not to ask Alyosha to remember anything they would not say in the group.

The notes are not mixed with your own memory in either direction. You can read, edit, or delete the files with ordinary tools.

## Deferred

- Recall of past sessions, which requires persistent history.
- Background extraction of memories after a session.
- Per-workspace memory.
- Approval for writes after the turn has read untrusted content.
- A `/memory` command for listing and pruning.
- A way for a person to see or clear their own notes from the chat.
- Linking one person's IDs across platforms or QQ groups.

[Memory implementation guide](../develop/Memory.md)
