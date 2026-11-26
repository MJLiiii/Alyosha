# Implement Memory

Open one `MemoryStore` for the `owner` scope at startup. Build the memory index once per session and add it to the normal system instruction. Register the `memory` tool for direct model calls. [Memory design](../design/Memory.md) · [Architecture overview](../README.md)

## V1 store contract

```text
MemoryStore
    open(root: &Path, scope: &str) -> Result<MemoryStore, MemoryError>
    index(&self) -> MemoryIndex
    list(&self) -> Vec<MemoryEntry>
    read(&self, name: &str) -> Result<String, MemoryError>
    write(&self, name: &str, description: &str, content: &str)
        -> Result<WriteKind, MemoryError>
    delete(&self, name: &str) -> Result<(), MemoryError>

MemoryEntry { name: String, description: String, updated: Date }
MemoryIndex { text: String, omitted: usize, unreadable: Vec<String> }
WriteKind   Created | Updated
```

- **Location.** The root is `${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/memory`. The scope directory is `owner`. Create the directories with mode `0700` on first write. Create files with mode `0600`. A missing directory is an empty store.
- **Names.** A name matches `[a-z0-9][a-z0-9-]{0,63}` and maps to `<name>.md` in the scope directory. Reject every other name before touching the filesystem. Open files relative to the scope directory handle without following symlinks. Ignore other files in the directory.
- **File format.** Use a front matter block with `description` and `updated` (an ISO date), followed by the body. `write` sets `updated` to the local date. A file without a valid header is unreadable. `index` and `list` skip it and record its name in `unreadable`. `read` still returns its raw text so the model can rewrite it.
- **Limits.** A description holds at most 160 bytes on one line. A body holds at most 4 KiB. The index text has a 4 KiB budget.
- **Atomic writes.** Stage the complete file in the scope directory and rename it into place, as for workspace files. Concurrent sessions writing the same name resolve as last write wins. [Atomic file replacement](EnvironmentInterface.md#atomic-file-replacement)

## Build the index

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

The Context Manager counts this text as system instructions. Summary requests keep their own instructions and do not include the index. If `unreadable` is nonempty, show **N memory files could not be read** in the status area. [Context Manager budgets](ContextManager.md#v1-token-budgets) · [LLM Client caller contract](LLMClientGateway.md#v1-caller-contract)

## `memory` tool

| `command` | Other inputs | Successful output | User notice |
| --- | --- | --- | --- |
| `list` | none | One index line per readable entry, without the budget | none |
| `read` | `name` | The file text, including its header | none |
| `write` | `name`, `description`, `content` | `created` or `updated` | **Remembered: name** |
| `delete` | `name` | Acknowledgment | **Forgot: name** |

`write` replaces the whole memory. To change a memory, the model reads it and then writes the new version. Return a tool error in these cases:

- an invalid name, or a missing name for `read` or `delete`
- an oversized description or body
- a missing memory for `read` or `delete`
- a write that would grow the index past its budget, with a message that suggests merging or deleting memories
- text that matches the credential patterns below

Check `description` and `content` against these patterns before staging:

- `-----BEGIN ` … `PRIVATE KEY-----`
- `sk-` followed by 20 or more letters, digits, `-`, or `_`
- `ghp_`, `gho_`, or `github_pat_` tokens
- `AKIA` followed by 16 capitals or digits
- `xoxb-` and `xoxp-` tokens
- `AIza` followed by 35 key characters

The patterns are a guard against accidents, not a guarantee.

**Proposed interface addition.** `ToolOutput` carries an optional `notice: String`. The Agent Loop passes it to the session driver, which shows it in the status area outside history. The model sees only the tool output. [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

**Scope.** The session `ToolContext` carries the store handle. The tool is available to direct model calls only. The Code Mode host never exposes it, and the shell sandbox does not mount the memory root. [Tool permissions](../reference/ToolPermissions.md#memory-store)

## People's notes

Guest sessions use a second store for the notes Alyosha keeps on each person in group chats. Your sessions never open it, and guest sessions never open the `owner` scope. [People in group chats](../design/Memory.md#people-in-group-chats) · [Guest mode](Channels.md#guest-mode)

```text
UserNotes
    open(root: &Path, today) -> UserNotes        # <root>/users
    read(&self, user: &str) -> Result<Option<Notes>, ToolError>
    write(&self, user: &str, name: &str, notes: &str) -> Result<WriteKind, ToolError>

Notes { name: String, updated: Date, text: String }
Sender { id: String, name: String }
```

- **IDs.** A user ID is the bridge's platform, a hyphen, and the platform's ID: `[a-z]+-[A-Za-z0-9_-]{1,64}`, such as `telegram-123456789`. It maps to `<id>.md`. Reject every other ID. The ACP mode checks it when the prompt arrives, before it reaches the store.
- **Format.** Front matter with `name`, the sender's display name, and `updated`, then the notes. Names are cut to 64 characters with line breaks and control characters replaced by spaces, so a display name cannot add header lines.
- **Limits and writes.** Notes hold at most 2 KiB. Check them against the credential patterns of the `memory` tool. Create the directory and files with the same modes, stage and rename as the owner store does, and never follow a symlink.

The guest session's driver keeps the sender of each queued message. When a message's turn starts, the session records that sender as the **speaker**, then builds the opening user entry:

```text
<clock line>
Notes on this person (<id>, updated <date>):
<notes, or "No notes yet.">

<name>: <text>
```

The `user_memory` tool takes one input, `notes`, the person's complete new notes, and writes the speaker's file with the speaker's current name. It returns `saved` and the notice **Remembered: <name>**. It returns a tool error when there is no speaker, for notes over 2 KiB, and for text that matches a credential pattern. It cannot name a file, so the model can only change the notes of the person it is answering. It is registered only in guest sessions.

## Verification

Run these against a temporary memory root with a fixed clock.

- Start with no directory. The index reads **No memories yet.** and the first `write` creates the directory and file with their modes.
- Write `thesis-repo`, read it back, update it, and list. Check `created` then `updated`, the new date, one entry, and the two notices.
- Try the names `../x`, `A`, and a 65-character name, and a symlink `x.md` pointing outside the root. Each returns a tool error and leaves the filesystem unchanged.
- Write oversized descriptions and bodies, and a body containing an `sk-` key. Each is rejected and the file is unchanged.
- In a guest session, queue messages from `telegram-1` and `telegram-2`. Script a `user_memory` call in the second turn. Check that only `telegram-2.md` is written, with the second sender's name, and that the second turn's opening entry carried the first person's notes only in the first turn.
- Send a sender ID of `../x` and a name containing a line break. Check that the prompt is refused and that a name with a line break is written on one line.
- Fill the index to its budget. A new write is rejected and an update that keeps the index size succeeds. Exceed the budget by hand-editing files. Check the newest entries appear, followed by the omitted count.
- Add a file without a header. Check it is skipped in the index, counted in the startup status, and still readable with `read`.
- Write a memory during a session. Check the system instruction stays byte-identical until the next compaction, and that the instruction after it, and a new session's, shows the entry.
- From an `execute_code` script, check that `tools.memory` is undefined. From `run_command`, check that the memory root is absent.
- Fail a write before the rename. Check that the existing file keeps its content and that no temporary file remains.

[Architecture overview](../README.md)
