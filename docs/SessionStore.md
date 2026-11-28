# Durable sessions

A session's conversation survives a restart. Alyosha writes every history entry to a PostgreSQL database as it happens. When a bridge or its `alyosha acp` restarts, after a crash, a configuration change, or a container reboot, the conversation picks up where it stopped. A session ends only when you start a new one. There is no nightly fresh start. [Agent State](AgentState.md) · [Chat channels](Channels.md) · [Architecture overview](README.md)

## What is saved

| Saved | Lost on restart |
| --- | --- |
| Which session each chat is using | Messages waiting in the queue, which are listed after the restart but not answered |
| Every history entry: your messages, the model's replies and tool calls with provider replay data, and tool results | A turn in progress, which ends as stopped |
| Each compaction's summary and where verbatim history resumes | Open approvals and `ask_owner` questions |
| The text of each message waiting in the queue, until its turn starts | |
| | Background jobs, MCP servers and their state, such as browser tabs, and the dev shell |
| | Saved full outputs of shortened results (`log_path`) |

The same database holds your memory and the notes on people in group chats. Skills, `schedule.toml`, and `config.toml` stay files. [Memory](Memory.md) · [Scheduled tasks](Schedule.md)

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

**Resuming.** Alyosha loads the latest summary and the entries after it. Older entries stay in the database but not in memory. If the last reply asked for tool calls that have no results, Alyosha records each as an error: *Alyosha restarted before this call finished. It may or may not have run; check before repeating it.* The first reply after a resume starts with *Resumed after a restart.*, adding *The turn in progress was stopped, and messages sent while Alyosha was down may need resending.* when a turn was interrupted. Messages that were still waiting in the queue are listed after it, with the time each was sent, so you know which to send again:

```text
Resumed after a restart. The turn in progress was stopped, and messages sent while Alyosha was down may need resending.
Not answered before the restart:
- Fri 10:02 "and check the logs too"
- Fri 10:03 "also the backup"
```

Alyosha does not answer them on its own after the restart, because the conversation may have moved on, and a message such as "send it" could repeat an action. Guest sessions never show startup lines in their group, so a guest group gets no list.

## Without the nightly fresh start

The 04:00 fresh start was how a chat session got rid of what it should not carry for weeks. Each of those now has its own answer:

| Before, renewed nightly | Now |
| --- | --- |
| History | Compaction bounds what the model sees. After a restart, only the entries since the last summary are loaded. [Context Manager](ContextManager.md) |
| Memory and skills indexes | Rebuilt at each compaction and each resume. A compaction already changes the start of the conversation, so the instruction changing at the same moment costs little prompt caching. [Memory](Memory.md#what-the-model-sees) |
| Saved full outputs | Removed at each compaction for results the summary replaced, and at restart. Reading a removed `log_path` gives a tool error saying the output is gone. |
| Configuration, MCP servers, and the dev shell | Picked up when the bridge restarts, which no longer loses the conversation: `systemctl restart alyosha-telegram`. |

## The database

PostgreSQL runs as a service inside the NixOS container. It listens only on a Unix socket, not on any network address, and accepts the user `alyosha` by peer authentication, so there is no password to keep. Its data lives in the container's own state, and a daily dump goes to `/var/backup/alyosha/` on the host. [Where things live](Deployment.md#where-things-live) [Deployment](Deployment.md)

The model's commands run as `alyosha-run`, which has no role, in units that hide the socket, so the model cannot read or change its history or memory with `psql`. MCP servers run as `alyosha` and could connect, like everything else they can reach. [Tool permissions](reference/ToolPermissions.md#container-limits)

The database holds what was said, which the logs never do. [Logging](reference/Logging.md)

If the database cannot be reached, `alyosha acp` stops with an error instead of running without saving.

## Deferred

- Answering the messages that were waiting at a restart on their own, instead of listing them.
- An archive of chat messages as the apps show them, including group messages that do not mention Alyosha.
- Deleting old sessions. V1 keeps them all.
- Resuming background jobs, open approvals, or saved full outputs after a restart.
- Listing or choosing among a key's earlier sessions.

## Implementation

Write each history entry and checkpoint to PostgreSQL as the session driver commits it, and resume a session by its key. Build this at step 10 of the build order, after the bridges work, because it changes how `alyosha acp` and both bridges open sessions. [Agent State](AgentState.md#implementation) · [Architecture overview](README.md)

### Placement and libraries

Put the store in `src/session_store.rs` in the root `alyosha` crate. The bridges never connect to the database: they pass a key to `alyosha acp`. Move the store into its own crate only when a second package needs it.

Use [`tokio-postgres`](https://docs.rs/tokio-postgres) with the `with-serde_json-1` and `with-chrono-0_4` features, which fits the existing tokio runtime. Each process opens one connection when its first session opens and keeps it, because the session lock lives on that connection. `sqlx` was not chosen: its compile-time query checks need a database during the Nix build.

Connect to `host=/run/postgresql dbname=alyosha` as the user in `USER`, which peer authentication maps to the role of the same name. The environment variable `ALYOSHA_DATABASE`, a libpq connection string, replaces this for tests and for development outside the container, as `ALYOSHA_LOG` does for logging. If the connection fails, stop with **Could not reach the session database at /run/postgresql. Check `systemctl status postgresql`.** and the driver's error. `alyosha acp` writes it to standard error and exits. [Before logging starts](reference/Logging.md#before-logging-starts)

### Schema

Embed the migrations in the binary as numbered SQL files, `migrations/001_sessions.sql` and so on. On connecting, take `pg_advisory_lock(0)`, apply every file newer than `schema_version` in one transaction, then release the lock, so two processes starting together migrate once. A database newer than the binary is an error. Migration 2, `002_memory.sql`, also imports the memory files of earlier builds in its transaction, so connect takes the memory folder. [Import the old files](Memory.md#import-the-old-files)

```sql
create table schema_version (version integer not null);

create table sessions (
    id          bigserial primary key,
    key         text not null,
    cwd         text not null,
    created_at  timestamptz not null default now(),
    updated_at  timestamptz not null default now()
);
create index sessions_key on sessions (key, id desc);

create table history (
    session_id  bigint not null references sessions,
    seq         bigint not null,           -- 0, 1, 2, ... per session
    kind        text not null,             -- 'user' | 'assistant' | 'tool_result'
    message_id  bigint,                    -- user entries only
    entry       json not null,         -- json, not jsonb: kept exactly as written
    created_at  timestamptz not null default now(),
    primary key (session_id, seq)
);

create table checkpoints (
    session_id      bigint not null references sessions,
    first_kept_seq  bigint not null,
    summary         text not null,
    created_at      timestamptz not null default now(),
    primary key (session_id, first_kept_seq)
);
```

`002_memory.sql` adds the `memories` and `user_notes` tables. [Memory store](Memory.md#v1-store-contract) `004_recall.sql` adds the searchable `text` column and the `history_embeddings` table. [Recall schema](Recall.md#schema-and-text) `003_media.sql` adds the `media` table, which holds the images of every session, and the store gains `put_media` and `get_media`. [The media table](Media.md#the-media-table)

A key's current session is its row with the highest `id`. `entry` is `json` rather than `jsonb` because `jsonb` reorders object keys, and provider replay data goes back to the provider as it came.

### Entries

Derive `serde::Serialize` and `Deserialize` for `HistoryEntry` and the types under it, with `#[serde(tag = "type")]` on the enums. Store each entry as `{"v": 2, ...}`. Version 2 gives a `User` entry ordered `parts`, text and image references, instead of one `text`. [Images](Media.md#images) Read a version 1 `User` entry's `text` as one text part; other version 1 entries read unchanged. Reading an entry with an unknown `v` fails the load with an error naming the session, rather than guessing. `Replay.data` is already JSON and is stored as it is. Image bytes are not in the entry: they are in the `media` table, by hash. [The media table](Media.md#the-media-table)

### The store

```rust
// Each method returns a BoxFuture, like the other traits in the crate.
trait SessionStore: Send + Sync {
    /// Creates a session under `key` and locks it.
    fn create(&self, key: &str, cwd: &Path) -> Result<StoredSession, StoreError>;
    /// Locks and loads the key's current session, or returns None.
    fn load_current(&self, key: &str) -> Result<Option<StoredSession>, StoreError>;
    fn append(&self, session: SessionId, seq: u64, entry: &HistoryEntry) -> Result<(), StoreError>;
    fn save_checkpoint(&self, session: SessionId, first_kept_seq: u64, summary: &str) -> Result<(), StoreError>;
    /// Releases the lock.
    fn close(&self, session: SessionId) -> Result<(), StoreError>;
}

struct StoredSession {
    id: SessionId,
    key: String,
    checkpoint: Option<(u64, String)>,   // first_kept_seq, summary
    entries: Vec<HistoryEntry>,          // from first_kept_seq, or all
    next_seq: u64,
    next_message_id: u64,                // max stored message_id + 1
    updated_at: DateTime<Utc>,
}
```

`PgStore` implements it on the connection. `MemoryStore` implements it in memory for the scripted tests, so `cargo test` needs no database. Locking is `pg_try_advisory_lock(id)` on the process's connection. If it returns false, fail with `StoreError::Busy`, which front ends show as **This session is open in another Alyosha process.** Both `append` and `save_checkpoint` also set `sessions.updated_at`.

### Agent State changes

`SessionSetup` takes an optional `Persistence`: the store, the `StoredSession`, and whether it was resumed. The driver keeps the store, the session ID, and `base_seq: u64`. Without it, as in most scripted tests, history lives only in memory. In-memory `history[i]` is the entry with `seq = base_seq + i`. `ContextCheckpoint.first_kept_index` stays an index into the in-memory history. The store holds `first_kept_seq = base_seq + first_kept_index`. Start the session's `MessageId` counter at `next_message_id`, so a resumed session never reuses an ID. [Agent State fields](AgentState.md#session-fields)

- **Appending.** Where `run_turn` appends an entry, call `append` first and push only after it succeeds. On failure, end the turn as `Failed("Could not save history: …")`. Nothing is pushed, so the in-memory history matches the database.
- **Checkpoints.** Where the driver commits a checkpoint, for a turn or `/compact`, call `save_checkpoint` first. On failure keep the previous checkpoint and treat it as a failed compaction. [Manual compaction state](AgentState.md#manual-compaction-state)
- **Loading.** Set `base_seq` to the checkpoint's `first_kept_seq`, or 0 without one. Fill history from the loaded entries and set the checkpoint's `first_kept_index` to 0.

### After each compaction

When a checkpoint commits, also:

1. **Rebuild the system instruction** with fresh memory and skills indexes, as at session startup, through the `rebuild_instruction` function the session startup gives the driver. Guest sessions have none. A request the Context Manager already prepared goes as built. Later requests use the new instruction. [Memory index](Memory.md#build-the-index) · [Skills index](Skills.md#build-the-index)
2. **Remove replaced logs.** For each `ToolResult` between the old and the new `first_kept_index` whose metadata carries a `[log_path: …]` line, remove that log from the session log store and release its space. Background jobs' logs appear only in job status text, never in that metadata, so they stay until the session ends. [Session logs](EnvironmentInterface.md#session-logs)

A `read_file` of a removed or unknown `log_path` returns the tool error **<path> is gone. Saved output is removed when older conversation is summarized or Alyosha restarts.**

### Resume

Loading a session runs the normal session startup, with the loaded state in place of empty state. Before the first turn:

1. If the last entry is an `Assistant` entry whose tool calls lack results, append a `ToolResult` for each missing call, in order, through the store: `ToolOutcome::Error("Alyosha restarted before this call finished. It may or may not have run; check before repeating it.")`.
2. Put a startup warning first: *Resumed after a restart.* If step 1 appended anything, or the last entry is a `User` entry, add *The turn in progress was stopped, and messages sent while Alyosha was down may need resending.*
3. Take the session's rows from the `pending` table, oldest first, and delete them. If there are any, add the line *Not answered before the restart:* and one line each, `- <day> <HH:MM> "<text>"`, in the configured timezone, with the text on one line and cut to 100 characters with `…`.

### The waiting queue

Migration `005_pending.sql`:

```sql
create table pending (
    session_id    bigint not null references sessions,
    message_id    bigint not null,
    submitted_at  timestamptz not null,
    text          text not null,     -- the message's text, and "[+N attachments]"
    primary key (session_id, message_id)
);
```

`SessionStore` gains `add_pending(session, message_id, submitted_at, text)`, `remove_pending(session, message_id)`, and `take_pending(session)`, which returns the rows by `message_id` and deletes them. `MemoryStore` keeps them in a map.

- **Writes in order.** Queuing a message is not async, so the session driver gives each write to one task of its own, through a channel, which runs them in order: add when a message is queued, remove when its turn starts. A failed write is logged at warn and does not stop the session.
- **Kept at shutdown.** Ending a session, at shutdown or `session/new`, discards its queue in memory and leaves its rows. A shutdown is when they are needed. After `session/new` the old session is no longer any key's current session, so nothing loads it to list them.
- **Text only.** A message's attachments are counted, not kept; the list says `[+2 attachments]` after its text.

### ACP

Change `initialize` to `loadSession: true`. [ACP mode](Channels.md#initialization)

- **`session/new`** reads the key from `_meta.alyosha.sessionKey`. Without one, use `acp:<random>`, a session nothing will resume. After ending any open session, `create` a session under the key and use the key as the `sessionId`.
- **`session/load`** with `sessionId`, `cwd`, and `mcpServers` treats `sessionId` as the key. End any open session first, as `session/new` does. Call `load_current`. With no session, answer the JSON-RPC error `-32002` "Resource not found" and open nothing. Otherwise start it in `cwd` with the normal startup, ignoring `mcpServers`. ACP requires the conversation to be replayed before the response, so send a `user_message_chunk` for each `User` entry's text parts without its clock line, with each image shown as `[image]`, and an `agent_message_chunk` for each assistant text part, in order, then answer with an empty result. `StoreError::Busy` is a JSON-RPC error with that message. [ACP session setup](https://agentclientprotocol.com/protocol/session-setup)

### Bridges

The shared `alyosha-bridge` crate opens a session in one call, `open_session(workspace, key)`: send `session/load`, and on `-32002` send `session/new` with the key in `_meta`. Drop every `session/update` that arrives before the `session/load` response, so the replay never reaches a chat. `new_session(workspace, key)` is the `/new` command. The guest pool builds a group's key from its client name: `alyosha-telegram` gives `guest:telegram:<chat ID>`. `/new` sends `session/new` with the same key. [Package layout](TelegramBridge.md#package-layout)

| Session | Key |
| --- | --- |
| Private | `owner:telegram`, `owner:qq` |
| Trusted group | `group:telegram:<chat ID>`, `group:qq:<group_openid>` |
| Guest group | `guest:telegram:<chat ID>`, `guest:qq:<group_openid>` |

### Container

`nix/container.nix` adds the database to the container. Pin the major version: moving between majors needs a dump and restore. [Host module](Deployment.md#host-module)

```nix
services.postgresql = {
  enable = true;
  package = pkgs.postgresql_17;
  enableTCPIP = false;
  settings.listen_addresses = lib.mkForce "";
};
```

- **`alyosha-db.service`**, a oneshot that runs as `postgres` after PostgreSQL starts, creates the login role `alyosha` and the `alyosha` database owned by it, each only if missing. NixOS's `ensureDBOwnership` only covers a database named after its user, so it does not fit.
- **`alyosha-db-backup.service`** runs daily as root: `runuser -u postgres -- pg_dump alyosha | gzip` into `/var/backup/alyosha/alyosha-<date>.sql.gz`, mode `0600`, written to a temporary name and renamed, keeping 7 days. The folder is the host's, mounted writable at the same path.
- **The bridge services** require and start after `alyosha-db.service`.

NixOS's default `local` rule authenticates by peer, so `alyosha` connects as its own role without a password. The data stays in the container's state under `/var/lib/nixos-containers/alyosha` on the host, which survives restarts and rebuilds. `nixos-container destroy` removes it, and the dumps are the copy that survives that. Restore with `alyosha backup restore <file>`, which empties the database and loads the dump.

Command units hide `/run/postgresql`, and `alyosha-run` has no role. [Command units](Deployment.md#command-units)

### Verification

These run under `cargo test` with `MemoryStore` and the scripted model. [Turn fixture](AgentLoop.md#verification-with-a-scripted-model-and-fake-tools)

The scripted cases are in `tests/session_store.rs`, `tests/session.rs`, `tests/acp.rs`, and the unit tests of `session_store`, `bridge`, and the log store. The PostgreSQL cases are in `tests/pg_store.rs`.

- Run the two-file turn. Check that the store holds the user entry, both assistant entries, and both results, with `seq` 0 to 4, and that each was stored before it appeared in memory.
- Make `append` fail on the second tool result. Check that the turn ends `Failed` with "Could not save history", that memory and store both hold the first result and not the second, and that the next turn runs.
- Script a compaction. Check that the store's checkpoint has `first_kept_seq = base_seq + first_kept_index`, that the next request's system instruction includes a memory written before the compaction, and that a `log_path` from a replaced result now reads as gone.
- Resume a session with a checkpoint. Check that memory holds only entries from `first_kept_seq`, that the next turn's request matches what it would have been without the restart, and that new `MessageId`s are above every stored one.
- Store a session whose last entry asks for two tool calls with one result. Resume it. Check for one appended error result for the second call, under its original call ID, and the stopped-turn line in the startup warning.
- Queue two messages behind a turn that waits, with one carrying an attachment. Check two `pending` rows, then end the session without answering and resume it. Check the stopped-turn line, *Not answered before the restart:* with both messages in order, their times, and `[+1 attachments]`, and that the rows are gone. Resume again. Check no list. Let a queued message's turn start. Check that its row is removed.
- Load a session twice from one store. Check that the second gets `Busy`.
- Over ACP: `initialize` reports `loadSession: true`. `session/load` of an unknown key returns `-32002`. `session/new` with a key and two turns, then a new process with `session/load` of the key: check the replayed chunks without clock lines, the empty result, and that a follow-up prompt sees the earlier turns.
- In the shared bridge crate with a fake agent, check that `open_session` falls back to `session/new` with the key and drops updates received before the load response.

With PostgreSQL, using `ALYOSHA_DATABASE`. These start a throwaway cluster with `initdb` and `pg_ctl` in a temporary directory when those are on `PATH`, and print a skip notice otherwise:

- Run the migrations from two processes at once. Check one `schema_version` row per file. Open the database with a binary that has fewer migrations and check for the newer-database error.
- Append, checkpoint, close, and load again on `PgStore`, and check the loaded entries, counters, and checkpoint. Check that `Replay` data survives byte for byte as JSON.
- Hold a session in one connection and try it from another. Check `Busy`. Close the first connection without `close` and check the second can now open it.

In the container:

- Kill `alyosha acp` under the Telegram bridge during a tool call. Check that the bridge restarts, the next reply starts with the resume line, and a follow-up question about the earlier conversation is answered.
- Restart the container. Check that both bridges resume their sessions and a guest group keeps its conversation.
- From `run_command`, run `ls /run/postgresql` and `psql alyosha`. Check that both fail.
- Run `alyosha backup now`. Check that a dump appears in `/var/backup/alyosha/` on the host, readable only by root, and that `alyosha backup restore` loads it into an emptied database.

[Architecture overview](README.md)
