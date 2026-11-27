# Recall of past sessions

Every conversation is saved in the session database, but the model sees only the current session since its last summary. Recall lets the model search the rest: what a compaction summarized away, sessions before a `/new`, and, from your private chat, your trusted groups. It finds what was said by the words used and by meaning, and reads the entries around a hit. [Durable sessions](SessionStore.md) · [Memory](Memory.md) · [Context Manager](ContextManager.md) · [What Alyosha still needs](todo.md)

Memory and recall do different jobs. A memory is a fact the model chose to keep, shown in every session's index. Recall finds anything that was said or seen, but only when the model goes looking. "What did I ask you about the router last week?" or "the address from that email you summarized" are recall. "I prefer answers in Chinese" is a memory.

## What the model can search

The `recall` tool searches the history entries of every saved session it may see. Entries the current session still has in context are left out, because the model already sees them.

| The session searching | Searches the sessions under |
| --- | --- |
| Your private chat, `owner:telegram` or `owner:qq` | Both private chats and every trusted group, `owner:*` and `group:*` |
| A trusted group | That group's key only |
| A guest group | That group's key only |

People in a trusted group can ask Alyosha to search, and the model answers in the group, so a group never searches your private chats. The other direction is open: your private chat already shares your memory with trusted groups, and what was said in a group was said in front of its members.

Every kind of entry is searched: your messages, including voice transcripts and the lines naming attachments; the model's replies and the names and arguments of its tool calls; and tool results, such as emails, pages, and command output. Images are not searched. Tool results are untrusted content when they are read again, as they were the first time. [Tool permissions](reference/ToolPermissions.md)

## How it searches

A search runs two ways at once and merges the results:

- **By the words.** Each word of the query, split on spaces, must appear in the entry's text, ignoring case. This finds exact names, numbers, and addresses, in Chinese as well as English, because it matches substrings and needs no word splitting.
- **By meaning.** OpenAI's `text-embedding-3-small`, through GPROXY with the same OpenAI key as transcription, turns the query and each entry into a vector, and the closest entries match. This finds "the router problem" when the conversation said "Wi-Fi kept dropping." [Transcription](Media.md#transcription)

The two lists are merged by rank, and the best eight hits are returned, each with its date, where it was said, and a short excerpt. The model reads around a hit with `read`.

An entry gets its vector the first time a search needs it: before searching, Alyosha computes the vectors that are missing in the search's scope. Writing history never waits for OpenAI, and if OpenAI or GPROXY fails, the search still answers by the words and says that meaning search was unavailable. The first search after this ships computes the vectors of all earlier history, about 500 entries on ms02 on 2026-11-27. Without `[recall] embed_provider`, searches use only the words.

Search by meaning sends the text of the history to OpenAI, including emails and pages from tool results. The model already reads all of it through the model provider.

## Guidance

The normal and guest instructions each get one line: **When the conversation refers to something earlier that you cannot see, search with `recall` before saying you do not know. What you find was true when it was said.**

## Deferred

- A vector index such as HNSW. An exact scan is fast enough for tens of thousands of entries.
- Splitting long entries. Only the start of a long entry gets a vector; search by the words still covers all of it.
- Filters by date, by chat, or by kind of entry.
- A local embedding model, so history does not leave the machine.
- Recall from Code Mode, and a `/recall` command in the chat.

## Implementation

Add a `recall` module in the root crate with the search, the embedding client, and the `recall` tool. Add migration `004_recall.sql` and the pgvector extension. Register the tool for direct model calls in every session, guest sessions included. [Built-in tools](reference/BuiltInTools.md#recall) · [Schema](SessionStore.md#schema)

### Schema and text

```sql
create extension if not exists vector;

alter table history add column text text;   -- the searchable text of the entry

create table history_embeddings (
    session_id  bigint not null,
    seq         bigint not null,
    model       text not null,
    embedding   vector(1536) not null,
    primary key (session_id, seq, model),
    foreign key (session_id, seq) references history
);
```

`HistoryEntry::search_text()` gives an entry's text: a user entry's text parts joined by line breaks, without the clock line; an assistant entry's text, then one line per tool call, `<name> <arguments as JSON>`; a tool result's output or error. `append` writes it into `history.text` with the entry. Migration 4 fills it for existing rows in its transaction from the Rust side, as migration 2 imports the memory files. Vectors are keyed by model, so a changed `embed_model` gets new vectors on the next search, and the old ones stay unused.

The container's PostgreSQL loads `pgvector` through `services.postgresql.extensions`. [Deployment](Deployment.md#implementation)

### Search

```text
RecallScope   Owner | OnlyKey(String)           # owner:* and group:*, or one key
RecallStore   # trait on the session store's connection; MemoryStore implements it for tests
    words(scope, exclude, terms, limit) -> Vec<Hit>
    missing_vectors(scope, model, limit) -> Vec<(EntryRef, String)>
    put_vectors(model, Vec<(EntryRef, Vec<f32>)>)
    nearest(scope, exclude, model, vector, limit) -> Vec<Hit>
    around(entry, before, after) -> Vec<StoredEntry>
EntryRef      { session_id, seq }               # shown to the model as "<session_id>:<seq>"
```

- **Scope.** An owner session passes `Owner`; a trusted group or guest session passes `OnlyKey` with its own key. `exclude` is the current session from its `first_kept_seq` on, or all of it before its first compaction.
- **Words.** `text ilike '%' || term || '%'` for each term, escaping `%`, `_`, and `\`, joined with `and`. Newest first, at most 20.
- **Vectors.** Before `nearest`, take up to 2,000 entries in scope without a vector for the model, oldest first, and embed them in batches of 100 with `POST <gateway url>/<embed_provider>/v1/embeddings`, `{"model": ..., "input": [...]}`, with the gateway key and the model requests' connect and response limits. Cut each text to its first 6,000 characters, and skip empty ones. Then embed the query and take the 20 nearest by cosine distance, `embedding <=> $1`. Through GPROXY 4.2.5 on ms02 on 2026-11-27, a request for two inputs returned two vectors.
- **Failure.** If embedding fails, keep the vectors already stored, search by the words, and start the result with **Meaning search unavailable: <error>.**
- **Merge.** Score each hit `1/(60 + rank)` in each list it appears in, add, and keep the best 8. Ties go to the newer entry.

### `recall` tool

| `command` | Other inputs | Output |
| --- | --- | --- |
| `search` | `query` | Up to 8 hits, best first: `<ref> <date> <where> <kind>` and an excerpt of at most 300 characters, around the first matching word or from the start. Or **Nothing found.** |
| `read` | `ref`, `before?`, `after?` | The entry and up to 3 entries before and after it in the same session, by default, at most 10 each way, each with its seq, date, and kind |

`<where>` is the chat, such as `private Telegram chat` or the trusted group's configured name, and the session's start date. `read` refuses a `ref` outside the session's scope with the same error as one that does not exist, **No entry <ref>.** Long results follow the usual limits and are saved in the session's logs for `read_file`. [Large tool results](ContextManager.md#large-tool-results) The tool needs no approval, shows no status line, and is not exposed to Code Mode. [Tool permissions](reference/ToolPermissions.md)

### Configuration

```toml
[recall]
embed_provider = "openai"                 # a GPROXY provider on the openai channel; none means words only
embed_model = "text-embedding-3-small"
```

The provider may be `[media] transcribe_provider`, which `alyosha gateway login transcription` already set up. [Configuration](reference/Configuration.md#recall)

### Verification

Scripted, with `MemoryStore` and a fake embedding server:

- Two sessions under `owner:telegram`, the first before a `/new`. Search a word from the first. Check the hit's ref, date, and excerpt, and that `read` returns the entries around it.
- In a session with a checkpoint, search a word said before and one said after `first_kept_seq`. Check that only the first is found.
- Search a two-character Chinese word and a mixed-case English word. Check both are found.
- From `owner:qq`, find a word said in a trusted group. From that group, search a word said in the private chat and check **Nothing found.**; `read` its ref and check **No entry <ref>.** From a guest group, check the same for both.
- Search for a word that appears only in a tool result and only in a tool call's arguments. Check both are found.
- The fake server returns vectors that rank an entry without the query's words first. Check the merged order. Make it fail. Check the words-only result and its first line, and that the next search embeds the missing entries.
- Store 2,500 entries without vectors. Check one search embeds 2,000 in batches of 100, and the next the remaining 500.
- Change `embed_model`. Check the next search embeds again under the new model.
- Run migration 4 on a version 3 database with entries. Check `text` is filled and the clock line is not in it.

With PostgreSQL and pgvector: the migration on a throwaway server, then `nearest` against stored vectors.

On ms02: ask from the private chat "what did we talk about the first day?" and check the model calls `recall` and answers from the earlier session.
