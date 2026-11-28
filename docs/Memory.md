# Memory

Memory holds facts that outlast a session. Examples are your preferences, facts about you, corrections you have given, and stable facts about your machines and projects. Session history ends with the session. Memory starts in the next one. [Agent State](AgentState.md) · [Architecture overview](README.md)

Memory stores **curated facts only**. Searching past conversations is [recall](Recall.md), a separate tool. Alyosha does not learn procedures. Procedures you write yourself are [skills](Skills.md). Every memory is one you or the model wrote on purpose.

## Where memories live

Memories live in the session database, one row per memory. Each has a name, a one-line description, the date of the last update, and the body, which holds the fact.

```text
name         thesis-repo
description  Thesis repository location and build command
updated      2026-11-24
body         The thesis lives in ~/thesis. Build it with `latexmk -pdf main.tex`.
```

The database keeps memory consistent across the sessions that share it and puts it in the daily dump with the conversations. You do not edit memories by hand. The model keeps them, and you can ask it to change or forget one. To look at them directly, run `alyosha memory list` on the host. [The host command](Deployment.md#the-host-command) [The database](SessionStore.md#the-database)

The store is global. Every session sees the same memories. They hold what Alyosha knows about you. Your sessions, in your private chats, see them, and so do the sessions of group chats you list as trusted. Guest sessions in other group chats never see them. In every group chat, Alyosha also keeps notes on each person. [People in group chats](#people-in-group-chats)

Earlier versions kept memories and people's notes as files under `/var/lib/alyosha/memory`. The first start of the database version copies them in, once, checks the copy against the files, and leaves the files where they are. If a file cannot be copied, the bridges do not start until you fix it, so nothing is dropped unseen. [Move memory into the database](Deployment.md#move-memory-into-the-database) · [Import the old files](#import-the-old-files)

## What the model sees

When a session starts, Alyosha reads the store and builds an **index** with one line per memory: its name, description, and date. Alyosha appends the index to the normal system instruction, along with short guidance on using memory. The instruction stays the same between compactions, so compaction never removes it and provider prompt caching keeps working. A memory written during the session appears in the index after the next compaction or restart, when Alyosha rebuilds the index. [Durable sessions](SessionStore.md#without-the-nightly-fresh-start) The model already sees the write in its own history. [Default instruction](LLMClientGateway.md#default-instruction-for-normal-requests) · [Context Manager](ContextManager.md)

The index has a 4 KiB budget. The model reads a memory's full text when it needs it. For example, you ask “build my thesis”. The model sees `thesis-repo — Thesis repository location and build command` in the index and reads that memory before running the build.

## How memories are written

The model writes memories with the `memory` tool during a turn. It writes when you state something lasting, correct it, or tell it to remember something. It does not need your approval. Each write or delete shows in the chat's progress message, for example **Remembered: thesis-repo**, so injected instructions cannot change memory unseen. Alyosha sets the update date and keeps the index consistent. [Built-in tool contracts](reference/BuiltInTools.md#memory)

Two sessions can change the same memory at once, for example your private chat and a trusted group. To change an existing memory, the model must first read it in the same session. If another session changed it since then, the write fails and the model reads it again, so no session overwrites a change it has not seen.

The guidance tells the model to:

- update an existing memory instead of adding a near-duplicate
- skip facts that the workspace already records or that matter only to the current task
- treat each memory as true as of its date and verify it before relying on it for an action

The tool rejects text that looks like a credential, such as a private key or an API token.

Only direct model calls can use `memory`. Code Mode scripts and shell commands cannot reach the store. [Tool permissions](reference/ToolPermissions.md#memory-store)

## People in group chats

In group chats anyone can talk to Alyosha, and it keeps notes on each person in the session database, in trusted groups and guest groups alike. A person has one set of notes for every group they write in: the same notes come up in a trusted group and in a guest group. The notes are keyed by the platform and the person's user ID, such as `telegram-123456789` or `qq-<member_openid>`, because display names change and can be copied. [Group chats](Channels.md#group-chats)

```text
person   qq-6F0A…
name     Bob
updated  2026-11-25
notes    Prefers answers in Chinese. Studies physics in Delft.
```

- **Whole notes, not an index.** A person's notes are short, at most 2 KiB, so there is no index. Each message from that person reaches the model with their whole notes in front of it. A message from someone without notes says so.
- **Written only for the speaker.** The model rewrites the notes with the `user_memory` tool. Alyosha decides whose notes it writes, from the sender the bridge reports for the message being answered, so nobody can make Alyosha change another person's notes. The stored name is the person's display name at the time of the last write. If the same person's notes changed in another group since this message arrived, the write fails and returns the newer notes, and the model writes again with both. [Built-in tool contracts](reference/BuiltInTools.md#user_memory)
- **What goes in.** The guest instruction, and a line in a trusted group's, tells the model to note what helps it answer that person later: the language they prefer, what they work on, what they asked it to remember. It never stores secrets, and the tool rejects text that looks like a credential, as the `memory` tool does.
- **Not private from the group.** Notes come from what a person said in a group, and they enter that group's conversation when the person writes. Tell people not to ask Alyosha to remember anything they would not say in the group.

- **You are not one of them.** In a trusted group, the bridge recognizes your own messages: by the same ID as in your private chat: your `owner_id` in Telegram, and your `owner_openid` in QQ, which QQ also gives you as your `member_openid` in groups. Your messages there carry no notes, and what the model learns about you goes into your memory as in your private chat. Guest groups do not recognize you: there you are a person with notes like anyone. [QQ IDs](QQBridge.md#configuration)

In a trusted group the model has both: your memory, for you, your machines, and your projects, and the speaker's notes, for the person writing. A guest session has only the notes. Nothing moves from one to the other on its own.

## Deferred

- Background extraction of memories after a session.
- Per-workspace memory.
- Approval for writes after the turn has read untrusted content.
- A `/memory` command for listing and pruning.
- A way for a person to see or clear their own notes from the chat.
- Linking one person's IDs across platforms or QQ groups.

## Implementation

Open one `MemoryStore` on the session database at startup. Build the memory index once per session and add it to the normal system instruction. Register the `memory` tool for direct model calls. [Architecture overview](README.md)

### V1 store contract

```text
MemoryTables                     # trait: the two tables, each write one guarded statement
    memories() / memory(name)
    insert_memory(row) -> bool                 # false if the name exists
    update_memory(row, seen) -> Option<i64>    # the new version, or None
    delete_memory(name, seen) -> bool
    notes(person) / insert_notes(row) / update_notes(row, seen)

MemoryStore                      # one per session: the tables and the versions it saw
    open(tables, today) -> MemoryStore
    index(&self) -> MemoryIndex
    list(&self) -> Vec<MemoryEntry>
    read(&self, name) -> String                # records the version
    write(&self, name, description, content) -> WriteKind
    delete(&self, name)

MemoryRow   { name, description, body, updated: Date, version: i64 }
MemoryEntry { name, description, updated: Date }
MemoryIndex { text: String, omitted: usize }
WriteKind   Created | Updated
```

`PgStore` implements `MemoryTables` on the session store's connection, and the front end passes it to each session. `InMemoryTables` implements it in memory for the scripted tests, as the session store's `MemoryStore` does for history. [The store](SessionStore.md#the-store)

- **Table.** Migration `002_memory.sql` adds the table to the session database. [Schema](SessionStore.md#schema)

  ```sql
  create table memories (
      name         text primary key,
      description  text not null,
      body         text not null,
      updated      date not null,
      version      bigint not null default 1
  );
  ```

- **Names.** A name matches `[a-z0-9][a-z0-9-]{0,63}`. Reject every other name before querying.
- **Dates.** `write` sets `updated` to the local date in the configured timezone.
- **Limits.** A description holds at most 160 bytes on one line. A body holds at most 4 KiB. The index text has a 4 KiB budget.
- **Versions.** Every write and delete is one statement guarded by the version the session last saw. The memory tool keeps, per session, the version of each memory it read or wrote. A write with no seen version inserts and fails if the name exists (`insert ... on conflict do nothing`). A write or delete with a seen version updates or deletes `where name = $1 and version = $2` and increments `version`. When no row matches, return a tool error: **thesis-repo changed since you read it, or was never read in this session. Read it again first.** The versions are not saved: a resumed session reads again before changing a memory.

### Build the index

Sort entries by `updated`, newest first, then by name. Emit one line per entry, `- <name> (<updated>): <description>`, while the text fits the 4 KiB budget. Count the rest in `omitted`. If anything was omitted, end with `N more memories are not shown. Use memory list.`

At session startup, build the normal system instruction once and keep it for the session.

```text
You are Alyosha, a personal assistant.

## Memory
You have a long-term memory shared across sessions. Use the memory tool to read a
memory before relying on it. Save lasting facts: the owner's preferences, facts about
the owner, corrections, and stable facts about their machines and projects. Update an
existing memory rather than adding a near-duplicate. Do not save facts the workspace
already records, details of the current task only, or secrets. Each memory is true as
of its date; verify it before acting on it.

<index lines, or "No memories yet.">
```

The Context Manager counts this text as system instructions. Summary requests keep their own instructions and do not include the index. [Context Manager budgets](ContextManager.md#v1-token-budgets) · [LLM Client caller contract](LLMClientGateway.md#v1-caller-contract)

### `memory` tool

| `command` | Other inputs | Successful output | User notice |
| --- | --- | --- | --- |
| `list` | none | One index line per readable entry, without the budget | none |
| `read` | `name` | The description, date, and body | none |
| `write` | `name`, `description`, `content` | `created` or `updated` | **Remembered: name** |
| `delete` | `name` | Acknowledgment | **Forgot: name** |

`write` replaces the whole memory. To change a memory, the model reads it and then writes the new version, as the versions above require. Return a tool error in these cases:

- an invalid name, or a missing name for `read` or `delete`
- an oversized description or body
- a missing memory for `read` or `delete`
- a write or delete whose version check fails
- a write that would grow the index past its budget, with a message that suggests merging or deleting memories
- text that matches the credential patterns below

Check `description` and `content` against these patterns before writing:

- `-----BEGIN ` … `PRIVATE KEY-----`
- `sk-` followed by 20 or more letters, digits, `-`, or `_`
- `ghp_`, `gho_`, or `github_pat_` tokens
- `AKIA` followed by 16 capitals or digits
- `xoxb-` and `xoxp-` tokens
- `AIza` followed by 35 key characters

The patterns are a guard against accidents, not a guarantee.

**Proposed interface addition.** `ToolOutput` carries an optional `notice: String`. The Agent Loop passes it to the session driver, which shows it in the status area outside history. The model sees only the tool output. [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

**Scope.** The session `ToolContext` carries the store handle. The tool is available to direct model calls only. The Code Mode host never exposes it, and the shell sandbox cannot reach the database socket. [Tool permissions](reference/ToolPermissions.md#memory-store)

### Notes on people

Group sessions use a second store for the notes Alyosha keeps on each person in group chats. Your private sessions never open it, and guest sessions never open the `owner` scope. [People in group chats](#people-in-group-chats) · [Guest mode](Channels.md#guest-mode)

```text
UserNotes                        # one per group session, on the same MemoryTables
    open(tables, today) -> UserNotes
    read(&self, user: &str) -> Result<Option<Notes>, ToolError>
    opening(&self, sender: &Sender, text: &str) -> String   # records the version shown
    write(&self, user: &str, name: &str, notes: &str) -> Result<WriteKind, ToolError>

Notes { name: String, updated: Date, text: String, version: i64 }
Sender { id: String, name: String, owner: bool }   // owner: only in trusted groups
```

The same migration adds their table:

```sql
create table user_notes (
    person   text primary key,           -- telegram-123456789, qq-<member_openid>
    name     text not null,
    notes    text not null,
    updated  date not null,
    version  bigint not null default 1
);
```

- **IDs.** A user ID is the bridge's platform, a hyphen, and the platform's ID: `[a-z]+-[A-Za-z0-9_-]{1,64}`, such as `telegram-123456789`. Reject every other ID. The ACP mode checks it when the prompt arrives, before it reaches the store.
- **Names.** The stored name is the sender's display name, cut to 64 characters with line breaks and control characters replaced by spaces.
- **Limits and writes.** Notes hold at most 2 KiB. Check them against the credential patterns of the `memory` tool. The notes shown at the start of the turn carry their version, and `write` is guarded by it as in the owner store. When the check fails, return a tool error holding the current notes, so the model can merge and write again.

A group session's driver keeps the sender of each queued message. When a message's turn starts, the session records that sender as the **speaker**, then builds the opening user entry:

```text
<clock line>
Notes on this person (<id>, updated <date>):
<notes, or "No notes yet.">

<name>: <text>
```

In a trusted group, a message from the owner opens with `<name> (owner): <text>` and no notes, and a message without a sender, such as a job's result or `/compact`, opens with its text alone.

The `user_memory` tool takes one input, `notes`, the person's complete new notes, and writes the speaker's notes with the speaker's current name. It returns `saved` and the notice **Remembered: <name>**. It returns a tool error when there is no speaker, for notes over 2 KiB, and for text that matches a credential pattern. It cannot name a person, so the model can only change the notes of the person it is answering. When the speaker is the owner, it returns the tool error **This is your owner. Keep what you learn about them with the memory tool.** It is registered in guest sessions and trusted groups' sessions.

**Who sees which store.** Both tables are in the same database, and every session connects as the same `alyosha` role, so the database does not keep owner and guest apart. The session does, by what it opens and registers: your private sessions get `MemoryStore`, the `memory` tool, and the index, and never `UserNotes`; trusted groups' sessions get both; guest sessions get `UserNotes` and `user_memory`, and never `MemoryStore`, the `memory` tool, or the index. This is the same separation the file version had, when both scopes were folders readable by `alyosha`.

### Import the old files

Earlier versions wrote each memory to `<state>/memory/owner/<name>.md` and each person's notes to `<state>/memory/users/<id>.md`, where `<state>` is `/var/lib/alyosha` in the container. Migration 2 imports them inside the transaction that applies `002_memory.sql`. It therefore runs once, under the migration lock, and either imports everything or nothing. [Schema](SessionStore.md#schema)

The session store's connect takes the memory folder, `<state>/memory`, and the migration step for version 2 runs the import after the SQL. Tests pass a temporary folder.

- **What is read.** Each `.md` file directly in `owner/` becomes a `memories` row and each in `users/` a `user_notes` row. A missing `memory/`, `owner/`, or `users/` folder imports nothing from it, as on a new install. Anything else under `memory/`, such as another folder or a file without `.md`, is not imported and is named in a warning in the log.
- **The file format.** A file starts with `---`, a header of `key: value` lines, and `---` on a line of its own, followed by the body. A memory's header holds `description:` and `updated:`; a person's holds `name:` and `updated:`, a `YYYY-MM-DD` date. The row takes the file name without `.md` as its `name` or `person`, the header values trimmed of spaces, and the body unchanged as `body` or `notes`. `updated` is the file's date, not the day of the import, and `version` is 1.
- **Refuse rather than drop.** These fail the import: a file name that is not a valid memory name or user ID; a symlink or anything other than a regular file; text that is not UTF-8; a header that is missing, lacks a field, has an empty description, or has a date that does not parse. The transaction rolls back, so the tables stay empty and the database stays at version 1. `alyosha acp` stops with **Could not import memory from /var/lib/alyosha/memory:** and every bad file with its reason, one per line, such as `owner/Notes.md: not a valid memory name`. The bridges do not start until each file is fixed or moved out of the folder.
- **No write checks.** The size limits, index budget, and credential patterns of the tools are not applied. The files passed them when they were written, and the index omits what does not fit its budget.
- **Check before commit.** After inserting, read both tables back in the same transaction and compare them with the parsed files: the same number of rows as files in each table, and for each file a row with the same name or ID, description or display name, date, and body. Any difference fails the import as above, with the file and the field that differs. Then log at info: **Imported N memories and M people's notes from /var/lib/alyosha/memory.**
- **The files stay.** The import never changes, moves, or deletes a file, and nothing reads the folder afterward. You delete it after checking the move. Until then an earlier build still runs on the files, but without anything remembered after the move. [Move memory into the database](Deployment.md#move-memory-into-the-database)

A database restored from a dump is already at version 2 or later, so it imports nothing.

### The host's list

`alyosha memory list` on the host runs `alyosha memory-list` in the container as `alyosha`. It prints one line per memory, `<name> (<updated>): <description>`, sorted by name, then a last line **N memories · M people with notes**. It only reads. [The host command](Deployment.md#the-host-command)

### Verification

Run these against a test database (`ALYOSHA_DATABASE`, as in the session store checks) with a fixed clock. [Session store verification](SessionStore.md#verification)

- Start with an empty table. The index reads **No memories yet.**
- Write `thesis-repo`, read it back, update it, and list. Check `created` then `updated`, the new date, one entry, and the two notices.
- Try the names `../x`, `A`, and a 65-character name. Each returns a tool error and leaves the table unchanged.
- Write oversized descriptions and bodies, and a body containing an `sk-` key. Each is rejected and the row is unchanged.
- In session A, read `thesis-repo`. In session B, read and update it. Check that A's write and delete then fail with the version error and leave B's text, and that A succeeds after reading again. Check that writing an existing memory without reading it first fails.
- Resume a session that had read a memory. Check that its first write of that memory fails until it reads it again.
- In a guest session, queue messages from `telegram-1` and `telegram-2`. Script a `user_memory` call in the second turn. Check that only `telegram-2` is written, with the second sender's name, and that the second turn's opening entry carried the first person's notes only in the first turn.
- Change `telegram-2`'s notes from another session between the message and the write. Check that the write fails and its error holds the newer notes.
- Send a sender ID of `../x` and a name containing a line break. Check that the prompt is refused and that a name with a line break is stored on one line.
- Fill the index to its budget. A new write is rejected and an update that keeps the index size succeeds. Exceed the budget with rows inserted directly. Check the newest entries appear, followed by the omitted count.
- Write a memory during a session. Check the system instruction stays byte-identical until the next compaction, and that the instruction after it, and a new session's, shows the entry.
- From an `execute_code` script, check that `tools.memory` is undefined. From `run_command`, check that `/run/postgresql` is absent.
- Check that the daily dump holds the `memories` and `user_notes` rows.

**Separation.** Write the owner memory `marker` whose body holds `OWNER-7f3c`, and notes for `telegram-1` holding `GUEST-2c9d`.

- In your session, script the model to call `memory` `list` and then `read` on `marker`. Check that the tools offered include `memory` and not `user_memory`, and that no request to the model contains `GUEST-2c9d`.
- In a guest session, queue a message from `telegram-1` and script a `user_memory` call. Check that the tools offered include `user_memory` and not `memory`, that the system instruction has no `## Memory` section, and that no request contains `OWNER-7f3c` or the index line for `marker`.
- In a trusted group's session, check that the index shows `marker` and the tools include `memory` and not `user_memory`.

**Import.** Run these with a temporary memory folder, a fresh test database, and a fixed clock dated after the files.

- With no memory folder, open the store. Check that both tables are empty, the database is at version 2, and nothing is logged about an import.
- Give the folder `owner/thesis-repo.md` with a body of several lines and a trailing blank line, `owner/editor.md`, `owner/notes.txt`, `users/telegram-1.md`, and `users/qq-6F0A.md`. Open the store. Check that each row equals its file field for field, with the file's date and version 1; that the log says **Imported 2 memories and 2 people's notes**; that a warning names `owner/notes.txt`; and that every file's bytes and modification time are unchanged.
- Change `owner/editor.md` and open the store again. Check that the row still holds the first text and that no import is logged.
- Open with the folder present from two processes at once. Check one import and one log line.
- Into fresh databases, import a folder that adds, in turn, `owner/Bad.md`, a symlink `owner/link.md`, `users/telegram-3.md` without `updated:`, and `owner/empty.md` with an empty description. Each time check that opening fails with the import error naming that file and its reason, that both tables are empty, and that the database is at version 1. Remove the file and check that the next open imports the rest.
- Make the read-back return a body that differs from its file. Check that the import fails naming the file and field, and the tables are empty.
- Restore a dump taken after the import into an emptied database, with the folder still present. Check that nothing is imported again.
- Run `alyosha memory-list` against the imported database. Check the two lines for `editor` and `thesis-repo`, in that order, and **2 memories · 2 people with notes**.

[Architecture overview](README.md)
