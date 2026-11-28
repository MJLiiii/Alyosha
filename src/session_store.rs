//! The session store: every history entry and context checkpoint, saved as
//! it is committed, so a restarted process can resume a session by its key.
//!
//! `PgStore` keeps them in PostgreSQL. `MemoryStore` keeps them in memory
//! for the scripted tests. `PgStore` also holds the memory tables, whose
//! migration imports the memory files of earlier builds.
//!
//! Design: `docs/SessionStore.md`. Implementation guide:
//! `docs/SessionStore.md#implementation`.

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
};

use chrono::{DateTime, Utc};
use serde_json::Value;
use tokio_postgres::{Client, NoTls, Row};

use crate::{
    BoxFuture,
    agent_state::{CallId, HistoryEntry, ToolOutcome, ToolResultEntry},
    memory::{MemoryRow, MemoryTables, NotesRow},
    recall::{EntryRef, Hit, RecallScope, cosine_distance, like_pattern, matches_all, vector_text},
};

/// The format version every stored entry carries. Version 1 entries, whose
/// user entries have `text` instead of `parts`, are still read.
pub const ENTRY_VERSION: u64 = 2;
/// Where Alyosha connects unless `ALYOSHA_DATABASE` says otherwise.
pub const DEFAULT_DATABASE: &str = "host=/run/postgresql dbname=alyosha";
/// The result recorded for a call a restart cut off.
pub const INTERRUPTED_CALL: &str = "Alyosha restarted before this call finished. It may or may not have run; check before repeating it.";
/// The first startup warning of a resumed session.
pub const RESUMED_LINE: &str = "Resumed after a restart.";
/// Added to it when a turn was in progress.
pub const INTERRUPTED_LINE: &str = "The turn in progress was stopped, and messages sent while Alyosha was down may need resending.";
/// What a front end shows for `StoreError::Busy`.
pub const BUSY: &str = "This session is open in another Alyosha process.";

/// The migrations, in order. Each runs once, in one transaction.
const MIGRATIONS: &[&str] = &[
    include_str!("../migrations/001_sessions.sql"),
    include_str!("../migrations/002_memory.sql"),
    include_str!("../migrations/003_media.sql"),
    include_str!("../migrations/004_recall.sql"),
    include_str!("../migrations/005_pending.sql"),
];
/// The migration that also imports the old memory files.
const MEMORY_MIGRATION: i32 = 2;
/// The migration that also fills each entry's searchable text.
const RECALL_MIGRATION: i32 = 4;

/// A message that was waiting in a session's queue.
/// See `docs/SessionStore.md#the-waiting-queue`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingRow {
    pub message_id: u64,
    pub submitted_at: DateTime<Utc>,
    pub text: String,
}

/// A stored session's database ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SessionId(pub i64);

/// Why the store could not do something.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StoreError {
    /// Another process holds the session.
    #[error("{BUSY}")]
    Busy,
    /// The database could not be reached or refused the request.
    #[error("{0}")]
    Failed(String),
    /// The old memory files could not be imported; the message names each
    /// file and why.
    #[error("{0}")]
    Import(String),
}

impl From<tokio_postgres::Error> for StoreError {
    fn from(error: tokio_postgres::Error) -> Self {
        StoreError::Failed(error.to_string())
    }
}

/// A session as loaded or created, locked by this process.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSession {
    pub id: SessionId,
    pub key: String,
    /// The latest checkpoint: where verbatim history resumes, and the summary.
    pub checkpoint: Option<(u64, String)>,
    /// The entries from the checkpoint's position, or all of them.
    pub entries: Vec<HistoryEntry>,
    pub next_seq: u64,
    /// One above every stored message ID.
    pub next_message_id: u64,
    /// When the session last changed.
    pub updated_at: DateTime<Utc>,
}

impl StoredSession {
    /// The sequence number of `entries[0]`.
    pub fn base_seq(&self) -> u64 {
        self.checkpoint.as_ref().map_or(0, |(seq, _)| *seq)
    }
}

/// Where sessions are saved. Each method's session must be locked by this
/// process, through `create` or `load_current`.
pub trait SessionStore: Send + Sync {
    /// Creates a session under `key` and locks it.
    fn create<'a>(
        &'a self,
        key: &'a str,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<StoredSession, StoreError>>;
    /// Locks and loads the key's current session, the newest under the key,
    /// or returns `None`.
    fn load_current<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<StoredSession>, StoreError>>;
    fn append<'a>(
        &'a self,
        session: SessionId,
        seq: u64,
        entry: &'a HistoryEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    fn save_checkpoint<'a>(
        &'a self,
        session: SessionId,
        first_kept_seq: u64,
        summary: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    /// Releases the lock.
    fn close(&self, session: SessionId) -> BoxFuture<'_, Result<(), StoreError>>;

    // The waiting queue: docs/SessionStore.md#the-waiting-queue

    fn add_pending<'a>(
        &'a self,
        session: SessionId,
        row: &'a PendingRow,
    ) -> BoxFuture<'a, Result<(), StoreError>>;
    fn remove_pending(
        &self,
        session: SessionId,
        message_id: u64,
    ) -> BoxFuture<'_, Result<(), StoreError>>;
    /// The session's rows by message ID, which are then deleted.
    fn take_pending(
        &self,
        session: SessionId,
    ) -> BoxFuture<'_, Result<Vec<PendingRow>, StoreError>>;

    // Recall: docs/Recall.md#search. `current` is the searching session,
    // whose entries from its latest checkpoint on are left out.

    /// The newest entries in scope holding every term, ignoring case.
    fn recall_words<'a>(
        &'a self,
        scope: &'a RecallScope,
        current: SessionId,
        terms: &'a [String],
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>>;
    /// The oldest entries in scope with text and no vector for `model`.
    fn recall_missing<'a>(
        &'a self,
        scope: &'a RecallScope,
        model: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<(EntryRef, String)>, String>>;
    fn recall_put<'a>(
        &'a self,
        model: &'a str,
        vectors: &'a [(EntryRef, Vec<f32>)],
    ) -> BoxFuture<'a, Result<(), String>>;
    /// The entries in scope nearest `vector` by cosine distance.
    fn recall_nearest<'a>(
        &'a self,
        scope: &'a RecallScope,
        current: SessionId,
        model: &'a str,
        vector: &'a [f32],
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>>;
    /// The entry and up to `before` and `after` around it in its session,
    /// or `None` if it does not exist or is out of scope.
    fn recall_around<'a>(
        &'a self,
        scope: &'a RecallScope,
        entry: EntryRef,
        before: u64,
        after: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<Hit>>, String>>;
}

/// An entry as stored: the entry's JSON with `"v": 2`.
pub fn encode(entry: &HistoryEntry) -> Value {
    let mut value = serde_json::to_value(entry).expect("history entries serialize");
    value["v"] = ENTRY_VERSION.into();
    value
}

/// Reads a stored entry, refusing a format this binary does not know.
pub fn decode(mut value: Value) -> Result<HistoryEntry, String> {
    let version = value.get("v").and_then(Value::as_u64);
    if !matches!(version, Some(1) | Some(ENTRY_VERSION)) {
        return Err(format!(
            "a history entry has format {}, which this Alyosha does not read",
            version.map_or_else(|| "none".to_owned(), |v| v.to_string())
        ));
    }
    if let Some(object) = value.as_object_mut() {
        object.remove("v");
    }
    serde_json::from_value(value).map_err(|error| format!("a history entry is unreadable: {error}"))
}

fn kind(entry: &HistoryEntry) -> &'static str {
    match entry {
        HistoryEntry::User(_) => "user",
        HistoryEntry::Assistant(_) => "assistant",
        HistoryEntry::ToolResult(_) => "tool_result",
    }
}

fn message_id(entry: &HistoryEntry) -> Option<u64> {
    match entry {
        HistoryEntry::User(user) => Some(user.message_id.0),
        _ => None,
    }
}

/// The lines that list the messages a restart left unanswered, after the
/// resume line, or nothing. See `docs/SessionStore.md#resume`.
pub fn unanswered_lines(waiting: &[PendingRow], timezone: chrono_tz::Tz) -> String {
    if waiting.is_empty() {
        return String::new();
    }
    let mut lines = String::from("\nNot answered before the restart:");
    for row in waiting {
        let one_line = row.text.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut text: String = one_line.chars().take(100).collect();
        if one_line.chars().count() > 100 {
            text.push('…');
        }
        lines.push_str(&format!(
            "\n- {} \"{text}\"",
            row.submitted_at.with_timezone(&timezone).format("%a %H:%M")
        ));
    }
    lines
}

/// Closes a turn a restart cut off: each tool call of the last reply that
/// has no result gets an error result, saved through the store. Returns
/// whether a turn was in progress, which is also the case when the last
/// entry is a message with no reply.
pub async fn repair(
    store: &dyn SessionStore,
    session: &mut StoredSession,
) -> Result<bool, StoreError> {
    let Some(last_reply) = session
        .entries
        .iter()
        .rposition(|entry| matches!(entry, HistoryEntry::Assistant(_)))
    else {
        return Ok(matches!(
            session.entries.last(),
            Some(HistoryEntry::User(_))
        ));
    };
    if matches!(session.entries.last(), Some(HistoryEntry::User(_))) {
        return Ok(true);
    }
    let HistoryEntry::Assistant(reply) = &session.entries[last_reply] else {
        unreachable!("found above");
    };
    let answered: HashSet<&CallId> = session.entries[last_reply + 1..]
        .iter()
        .filter_map(|entry| match entry {
            HistoryEntry::ToolResult(result) => Some(&result.call_id),
            _ => None,
        })
        .collect();
    let missing: Vec<CallId> = reply
        .tool_calls()
        .filter(|call| !answered.contains(&call.id))
        .map(|call| call.id.clone())
        .collect();
    for call_id in &missing {
        let entry = HistoryEntry::ToolResult(ToolResultEntry {
            call_id: call_id.clone(),
            outcome: ToolOutcome::Error(INTERRUPTED_CALL.into()),
            images: Vec::new(),
        });
        store.append(session.id, session.next_seq, &entry).await?;
        session.next_seq += 1;
        session.entries.push(entry);
    }
    Ok(!missing.is_empty())
}

/// Sessions in memory, for tests.
#[derive(Default)]
pub struct MemoryStore {
    inner: Mutex<MemoryInner>,
}

#[derive(Default)]
struct MemoryInner {
    next_id: i64,
    sessions: Vec<MemorySession>,
    locked: HashSet<i64>,
    /// Makes the next appends fail, by remaining count.
    fail_appends_after: Option<usize>,
    /// Recall's vectors by session, seq, and model.
    vectors: HashMap<(i64, i64, String), Vec<f32>>,
    /// The waiting queue's rows by session and message ID.
    pending: std::collections::BTreeMap<(i64, u64), PendingRow>,
}

struct MemorySession {
    id: i64,
    key: String,
    entries: HashMap<u64, Value>,
    /// When each entry was appended.
    times: HashMap<u64, DateTime<Utc>>,
    checkpoints: Vec<(u64, String)>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
}

impl MemorySession {
    /// The first seq the session's model still sees.
    fn visible_from(&self) -> u64 {
        self.checkpoints
            .iter()
            .map(|(seq, _)| *seq)
            .max()
            .unwrap_or(0)
    }

    fn hit(&self, seq: u64) -> Option<Hit> {
        let entry = decode(self.entries.get(&seq)?.clone()).ok()?;
        Some(Hit {
            entry: EntryRef {
                session_id: self.id,
                seq: seq as i64,
            },
            key: self.key.clone(),
            session_started: self.created_at,
            at: self.times[&seq],
            kind: kind(&entry).to_owned(),
            text: entry.search_text(),
        })
    }
}

impl MemoryInner {
    /// Every entry in scope, newest first, without the current session's
    /// visible ones.
    fn in_scope(&self, scope: &RecallScope, current: Option<SessionId>) -> Vec<Hit> {
        let mut hits: Vec<Hit> = self
            .sessions
            .iter()
            .filter(|session| scope.includes(&session.key))
            .flat_map(|session| {
                let hide_from =
                    (Some(SessionId(session.id)) == current).then(|| session.visible_from());
                session
                    .entries
                    .keys()
                    .filter(move |seq| hide_from.is_none_or(|from| **seq < from))
                    .filter_map(|seq| session.hit(*seq))
                    .collect::<Vec<_>>()
            })
            .collect();
        hits.sort_by(|a, b| {
            b.at.cmp(&a.at)
                .then(b.entry.session_id.cmp(&a.entry.session_id))
                .then(b.entry.seq.cmp(&a.entry.seq))
        });
        hits
    }
}

impl MemoryStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MemoryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lets `count` more appends succeed, then fails every later one.
    pub fn fail_appends_after(&self, count: usize) {
        self.lock().fail_appends_after = Some(count);
    }

    /// Every stored entry of a session, in order, for checks.
    pub fn entries(&self, session: SessionId) -> Vec<HistoryEntry> {
        let inner = self.lock();
        let Some(stored) = inner.sessions.iter().find(|s| s.id == session.0) else {
            return Vec::new();
        };
        let mut seqs: Vec<u64> = stored.entries.keys().copied().collect();
        seqs.sort_unstable();
        seqs.iter()
            .map(|seq| decode(stored.entries[seq].clone()).expect("stored by append"))
            .collect()
    }

    /// The sequence numbers stored for a session, in order.
    pub fn seqs(&self, session: SessionId) -> Vec<u64> {
        let inner = self.lock();
        let mut seqs: Vec<u64> = inner
            .sessions
            .iter()
            .find(|s| s.id == session.0)
            .map(|s| s.entries.keys().copied().collect())
            .unwrap_or_default();
        seqs.sort_unstable();
        seqs
    }

    /// The latest checkpoint of a session.
    pub fn checkpoint(&self, session: SessionId) -> Option<(u64, String)> {
        let inner = self.lock();
        inner
            .sessions
            .iter()
            .find(|s| s.id == session.0)
            .and_then(|s| s.checkpoints.iter().max_by_key(|(seq, _)| *seq).cloned())
    }

    /// Releases every lock, as when the holding process dies.
    pub fn drop_locks(&self) {
        self.lock().locked.clear();
    }

    /// The waiting queue's rows of a session, by message ID.
    pub fn pending(&self, session: SessionId) -> Vec<PendingRow> {
        self.lock()
            .pending
            .range((session.0, 0)..=(session.0, u64::MAX))
            .map(|(_, row)| row.clone())
            .collect()
    }

    /// How many vectors are stored for `model`.
    pub fn vector_count(&self, model: &str) -> usize {
        self.lock()
            .vectors
            .keys()
            .filter(|(_, _, stored)| stored == model)
            .count()
    }
}

impl SessionStore for MemoryStore {
    fn create<'a>(
        &'a self,
        key: &'a str,
        _cwd: &'a Path,
    ) -> BoxFuture<'a, Result<StoredSession, StoreError>> {
        Box::pin(async move {
            let mut inner = self.lock();
            inner.next_id += 1;
            let id = inner.next_id;
            inner.sessions.push(MemorySession {
                id,
                key: key.to_owned(),
                entries: HashMap::new(),
                times: HashMap::new(),
                checkpoints: Vec::new(),
                created_at: Utc::now(),
                updated_at: Utc::now(),
            });
            inner.locked.insert(id);
            Ok(StoredSession {
                id: SessionId(id),
                key: key.to_owned(),
                checkpoint: None,
                entries: Vec::new(),
                next_seq: 0,
                next_message_id: 1,
                updated_at: Utc::now(),
            })
        })
    }

    fn load_current<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<StoredSession>, StoreError>> {
        Box::pin(async move {
            let mut inner = self.lock();
            let Some(stored) = inner
                .sessions
                .iter()
                .filter(|s| s.key == key)
                .max_by_key(|s| s.id)
            else {
                return Ok(None);
            };
            let id = stored.id;
            let checkpoint = stored
                .checkpoints
                .iter()
                .max_by_key(|(seq, _)| *seq)
                .cloned();
            let from = checkpoint.as_ref().map_or(0, |(seq, _)| *seq);
            let mut seqs: Vec<u64> = stored.entries.keys().copied().collect();
            seqs.sort_unstable();
            let mut entries = Vec::new();
            let mut next_message_id = 1;
            for seq in &seqs {
                let entry = decode(stored.entries[seq].clone()).map_err(StoreError::Failed)?;
                if let Some(message) = message_id(&entry) {
                    next_message_id = next_message_id.max(message + 1);
                }
                if *seq >= from {
                    entries.push(entry);
                }
            }
            let loaded = StoredSession {
                id: SessionId(id),
                key: key.to_owned(),
                checkpoint,
                entries,
                next_seq: seqs.last().map_or(0, |seq| seq + 1),
                next_message_id,
                updated_at: stored.updated_at,
            };
            if !inner.locked.insert(id) {
                return Err(StoreError::Busy);
            }
            Ok(Some(loaded))
        })
    }

    fn append<'a>(
        &'a self,
        session: SessionId,
        seq: u64,
        entry: &'a HistoryEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.lock();
            if let Some(left) = inner.fail_appends_after.as_mut() {
                if *left == 0 {
                    return Err(StoreError::Failed(
                        "the test store refused the write".into(),
                    ));
                }
                *left -= 1;
            }
            let stored = inner
                .sessions
                .iter_mut()
                .find(|s| s.id == session.0)
                .ok_or_else(|| StoreError::Failed("no such session".into()))?;
            if stored.entries.contains_key(&seq) {
                return Err(StoreError::Failed(format!("entry {seq} is already stored")));
            }
            stored.entries.insert(seq, encode(entry));
            stored.times.insert(seq, Utc::now());
            stored.updated_at = Utc::now();
            Ok(())
        })
    }

    fn save_checkpoint<'a>(
        &'a self,
        session: SessionId,
        first_kept_seq: u64,
        summary: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let mut inner = self.lock();
            let stored = inner
                .sessions
                .iter_mut()
                .find(|s| s.id == session.0)
                .ok_or_else(|| StoreError::Failed("no such session".into()))?;
            stored.checkpoints.retain(|(seq, _)| *seq != first_kept_seq);
            stored
                .checkpoints
                .push((first_kept_seq, summary.to_owned()));
            stored.updated_at = Utc::now();
            Ok(())
        })
    }

    fn close(&self, session: SessionId) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            self.lock().locked.remove(&session.0);
            Ok(())
        })
    }

    fn add_pending<'a>(
        &'a self,
        session: SessionId,
        row: &'a PendingRow,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.lock()
                .pending
                .insert((session.0, row.message_id), row.clone());
            Ok(())
        })
    }

    fn remove_pending(
        &self,
        session: SessionId,
        message_id: u64,
    ) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            self.lock().pending.remove(&(session.0, message_id));
            Ok(())
        })
    }

    fn take_pending(
        &self,
        session: SessionId,
    ) -> BoxFuture<'_, Result<Vec<PendingRow>, StoreError>> {
        Box::pin(async move {
            let mut inner = self.lock();
            let keys: Vec<(i64, u64)> = inner
                .pending
                .range((session.0, 0)..=(session.0, u64::MAX))
                .map(|(key, _)| *key)
                .collect();
            Ok(keys
                .iter()
                .filter_map(|key| inner.pending.remove(key))
                .collect())
        })
    }

    fn recall_words<'a>(
        &'a self,
        scope: &'a RecallScope,
        current: SessionId,
        terms: &'a [String],
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            Ok(self
                .lock()
                .in_scope(scope, Some(current))
                .into_iter()
                .filter(|hit| matches_all(&hit.text, terms))
                .take(limit)
                .collect())
        })
    }

    fn recall_missing<'a>(
        &'a self,
        scope: &'a RecallScope,
        model: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<(EntryRef, String)>, String>> {
        Box::pin(async move {
            let inner = self.lock();
            let mut missing: Vec<(EntryRef, String)> = inner
                .in_scope(scope, None)
                .into_iter()
                .filter(|hit| !hit.text.is_empty())
                .filter(|hit| {
                    !inner.vectors.contains_key(&(
                        hit.entry.session_id,
                        hit.entry.seq,
                        model.to_owned(),
                    ))
                })
                .map(|hit| (hit.entry, hit.text))
                .collect();
            missing.sort_by_key(|(entry, _)| (entry.session_id, entry.seq));
            missing.truncate(limit);
            Ok(missing)
        })
    }

    fn recall_put<'a>(
        &'a self,
        model: &'a str,
        vectors: &'a [(EntryRef, Vec<f32>)],
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let mut inner = self.lock();
            for (entry, vector) in vectors {
                inner.vectors.insert(
                    (entry.session_id, entry.seq, model.to_owned()),
                    vector.clone(),
                );
            }
            Ok(())
        })
    }

    fn recall_nearest<'a>(
        &'a self,
        scope: &'a RecallScope,
        current: SessionId,
        model: &'a str,
        vector: &'a [f32],
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let inner = self.lock();
            let mut scored: Vec<(f64, Hit)> = inner
                .in_scope(scope, Some(current))
                .into_iter()
                .filter_map(|hit| {
                    let stored = inner.vectors.get(&(
                        hit.entry.session_id,
                        hit.entry.seq,
                        model.to_owned(),
                    ))?;
                    Some((cosine_distance(stored, vector), hit))
                })
                .collect();
            scored.sort_by(|(a, _), (b, _)| a.total_cmp(b));
            Ok(scored.into_iter().take(limit).map(|(_, hit)| hit).collect())
        })
    }

    fn recall_around<'a>(
        &'a self,
        scope: &'a RecallScope,
        entry: EntryRef,
        before: u64,
        after: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<Hit>>, String>> {
        Box::pin(async move {
            let inner = self.lock();
            let Some(session) = inner
                .sessions
                .iter()
                .find(|session| session.id == entry.session_id && scope.includes(&session.key))
            else {
                return Ok(None);
            };
            let Ok(seq) = u64::try_from(entry.seq) else {
                return Ok(None);
            };
            if !session.entries.contains_key(&seq) {
                return Ok(None);
            }
            let mut seqs: Vec<u64> = session
                .entries
                .keys()
                .copied()
                .filter(|other| *other + before >= seq && *other <= seq + after)
                .collect();
            seqs.sort_unstable();
            Ok(Some(
                seqs.into_iter()
                    .filter_map(|seq| session.hit(seq))
                    .collect(),
            ))
        })
    }
}

/// Sessions in PostgreSQL, over one connection that holds the locks.
pub struct PgStore {
    client: Client,
}

impl PgStore {
    /// Connects to `ALYOSHA_DATABASE`, or the container's database, and
    /// brings the schema up to date, importing the old memory files from
    /// `memory_folder` if it reaches migration 2.
    pub async fn connect_default(memory_folder: &Path) -> Result<Arc<Self>, StoreError> {
        let target =
            std::env::var("ALYOSHA_DATABASE").unwrap_or_else(|_| DEFAULT_DATABASE.to_owned());
        Self::connect(&target, memory_folder).await.map(Arc::new)
    }

    /// Connects with a libpq-style connection string. Without a user, it
    /// connects as `USER`, which peer authentication maps to its own role.
    pub async fn connect(target: &str, memory_folder: &Path) -> Result<Self, StoreError> {
        let mut config: tokio_postgres::Config = target
            .parse()
            .map_err(|error: tokio_postgres::Error| StoreError::Failed(error.to_string()))?;
        if config.get_user().is_none() {
            let user = std::env::var("USER")
                .or_else(|_| std::env::var("LOGNAME"))
                .map_err(|_| StoreError::Failed("USER is not set".into()))?;
            config.user(user);
        }
        let (mut client, connection) = config.connect(NoTls).await?;
        tokio::spawn(async move {
            if let Err(error) = connection.await {
                tracing::error!(error = %error, "the session database connection failed");
            }
        });
        migrate(&mut client, memory_folder).await?;
        Ok(Self { client })
    }

    /// How many people have notes.
    pub async fn count_notes(&self) -> Result<i64, StoreError> {
        Ok(self
            .client
            .query_one("select count(*) from user_notes", &[])
            .await?
            .get(0))
    }

    async fn try_lock(&self, id: i64) -> Result<(), StoreError> {
        let row = self
            .client
            .query_one("select pg_try_advisory_lock($1)", &[&id])
            .await?;
        if row.get::<_, bool>(0) {
            Ok(())
        } else {
            Err(StoreError::Busy)
        }
    }
}

/// Applies every migration newer than the database, under a lock so that
/// two processes starting together migrate once. Migration 2 also imports
/// the old memory files in its transaction.
async fn migrate(client: &mut Client, memory_folder: &Path) -> Result<(), StoreError> {
    client.execute("select pg_advisory_lock(0)", &[]).await?;
    let result = async {
        client
            .batch_execute("create table if not exists schema_version (version integer not null)")
            .await?;
        let current: i32 = client
            .query_one("select coalesce(max(version), 0) from schema_version", &[])
            .await?
            .get(0);
        let known = MIGRATIONS.len() as i32;
        if current > known {
            return Err(StoreError::Failed(format!(
                "the session database has schema version {current}, newer than this Alyosha's {known}"
            )));
        }
        for (index, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            let version = index as i32 + 1;
            let transaction = client.transaction().await?;
            transaction.batch_execute(sql).await?;
            if version == MEMORY_MIGRATION {
                crate::memory_import::import(&transaction, memory_folder).await?;
            }
            if version == RECALL_MIGRATION {
                fill_search_text(&transaction).await?;
            }
            transaction
                .execute("insert into schema_version (version) values ($1)", &[&version])
                .await?;
            transaction.commit().await?;
            tracing::info!(version, "applied a session database migration");
        }
        Ok(())
    }
    .await;
    let unlocked = client.execute("select pg_advisory_unlock(0)", &[]).await;
    result?;
    unlocked?;
    Ok(())
}

/// Fills `history.text` for the entries saved before migration 4.
async fn fill_search_text(transaction: &tokio_postgres::Transaction<'_>) -> Result<(), StoreError> {
    let rows = transaction
        .query("select session_id, seq, entry from history", &[])
        .await?;
    let update = transaction
        .prepare("update history set text = $3 where session_id = $1 and seq = $2")
        .await?;
    for row in &rows {
        let session: i64 = row.get(0);
        let seq: i64 = row.get(1);
        let entry = decode(row.get::<_, Value>(2))
            .map_err(|error| StoreError::Failed(format!("session {session}: {error}")))?;
        transaction
            .execute(&update, &[&session, &seq, &entry.search_text()])
            .await?;
    }
    tracing::info!(
        entries = rows.len(),
        "filled the searchable text of saved history"
    );
    Ok(())
}

/// A `memories` row selected as name, description, body, updated, version.
pub(crate) fn memory_row(row: &Row) -> MemoryRow {
    MemoryRow {
        name: row.get(0),
        description: row.get(1),
        body: row.get(2),
        updated: row.get(3),
        version: row.get(4),
    }
}

/// A `user_notes` row selected as person, name, notes, updated, version.
pub(crate) fn notes_row(row: &Row) -> NotesRow {
    NotesRow {
        person: row.get(0),
        name: row.get(1),
        notes: row.get(2),
        updated: row.get(3),
        version: row.get(4),
    }
}

fn text_error(error: tokio_postgres::Error) -> String {
    error.to_string()
}

impl MemoryTables for PgStore {
    fn memories(&self) -> BoxFuture<'_, Result<Vec<MemoryRow>, String>> {
        Box::pin(async move {
            let rows = self
                .client
                .query(
                    "select name, description, body, updated, version from memories",
                    &[],
                )
                .await
                .map_err(text_error)?;
            Ok(rows.iter().map(memory_row).collect())
        })
    }

    fn memory<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<MemoryRow>, String>> {
        Box::pin(async move {
            let row = self
                .client
                .query_opt(
                    "select name, description, body, updated, version from memories where name = $1",
                    &[&name],
                )
                .await
                .map_err(text_error)?;
            Ok(row.as_ref().map(memory_row))
        })
    }

    fn insert_memory<'a>(&'a self, row: &'a MemoryRow) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let inserted = self
                .client
                .execute(
                    "insert into memories (name, description, body, updated) values ($1, $2, $3, $4)
                     on conflict (name) do nothing",
                    &[&row.name, &row.description, &row.body, &row.updated],
                )
                .await
                .map_err(text_error)?;
            Ok(inserted == 1)
        })
    }

    fn update_memory<'a>(
        &'a self,
        row: &'a MemoryRow,
        seen: i64,
    ) -> BoxFuture<'a, Result<Option<i64>, String>> {
        Box::pin(async move {
            let updated = self
                .client
                .query_opt(
                    "update memories set description = $2, body = $3, updated = $4, version = version + 1
                     where name = $1 and version = $5 returning version",
                    &[&row.name, &row.description, &row.body, &row.updated, &seen],
                )
                .await
                .map_err(text_error)?;
            Ok(updated.map(|row| row.get(0)))
        })
    }

    fn delete_memory<'a>(
        &'a self,
        name: &'a str,
        seen: i64,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let deleted = self
                .client
                .execute(
                    "delete from memories where name = $1 and version = $2",
                    &[&name, &seen],
                )
                .await
                .map_err(text_error)?;
            Ok(deleted == 1)
        })
    }

    fn notes<'a>(&'a self, person: &'a str) -> BoxFuture<'a, Result<Option<NotesRow>, String>> {
        Box::pin(async move {
            let row = self
                .client
                .query_opt(
                    "select person, name, notes, updated, version from user_notes where person = $1",
                    &[&person],
                )
                .await
                .map_err(text_error)?;
            Ok(row.as_ref().map(notes_row))
        })
    }

    fn insert_notes<'a>(&'a self, row: &'a NotesRow) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let inserted = self
                .client
                .execute(
                    "insert into user_notes (person, name, notes, updated) values ($1, $2, $3, $4)
                     on conflict (person) do nothing",
                    &[&row.person, &row.name, &row.notes, &row.updated],
                )
                .await
                .map_err(text_error)?;
            Ok(inserted == 1)
        })
    }

    fn update_notes<'a>(
        &'a self,
        row: &'a NotesRow,
        seen: i64,
    ) -> BoxFuture<'a, Result<Option<i64>, String>> {
        Box::pin(async move {
            let updated = self
                .client
                .query_opt(
                    "update user_notes set name = $2, notes = $3, updated = $4, version = version + 1
                     where person = $1 and version = $5 returning version",
                    &[&row.person, &row.name, &row.notes, &row.updated, &seen],
                )
                .await
                .map_err(text_error)?;
            Ok(updated.map(|row| row.get(0)))
        })
    }
}

impl crate::media::MediaStore for PgStore {
    fn put_media<'a>(
        &'a self,
        mime: &'a str,
        bytes: &'a [u8],
    ) -> BoxFuture<'a, Result<String, String>> {
        Box::pin(async move {
            let hash = crate::media::sha256_hex(bytes);
            self.client
                .execute(
                    "insert into media (sha256, mime, bytes) values ($1, $2, $3) on conflict do nothing",
                    &[&hash, &mime, &bytes],
                )
                .await
                .map_err(text_error)?;
            Ok(hash)
        })
    }

    fn get_media<'a>(
        &'a self,
        sha256: &'a str,
    ) -> BoxFuture<'a, Result<Option<crate::media::store::MediaRow>, String>> {
        Box::pin(async move {
            Ok(self
                .client
                .query_opt("select mime, bytes from media where sha256 = $1", &[&sha256])
                .await
                .map_err(text_error)?
                .map(|row| (row.get(0), row.get(1))))
        })
    }
}

impl SessionStore for PgStore {
    fn create<'a>(
        &'a self,
        key: &'a str,
        cwd: &'a Path,
    ) -> BoxFuture<'a, Result<StoredSession, StoreError>> {
        Box::pin(async move {
            let row = self
                .client
                .query_one(
                    "insert into sessions (key, cwd) values ($1, $2) returning id, updated_at",
                    &[&key, &cwd.display().to_string()],
                )
                .await?;
            let id: i64 = row.get(0);
            self.try_lock(id).await?;
            Ok(StoredSession {
                id: SessionId(id),
                key: key.to_owned(),
                checkpoint: None,
                entries: Vec::new(),
                next_seq: 0,
                next_message_id: 1,
                updated_at: row.get(1),
            })
        })
    }

    fn load_current<'a>(
        &'a self,
        key: &'a str,
    ) -> BoxFuture<'a, Result<Option<StoredSession>, StoreError>> {
        Box::pin(async move {
            let Some(row) = self
                .client
                .query_opt(
                    "select id, updated_at from sessions where key = $1 order by id desc limit 1",
                    &[&key],
                )
                .await?
            else {
                return Ok(None);
            };
            let id: i64 = row.get(0);
            let updated_at: DateTime<Utc> = row.get(1);
            self.try_lock(id).await?;
            let loaded = async {
                let checkpoint = self
                    .client
                    .query_opt(
                        "select first_kept_seq, summary from checkpoints where session_id = $1 order by first_kept_seq desc limit 1",
                        &[&id],
                    )
                    .await?
                    .map(|row| (row.get::<_, i64>(0) as u64, row.get::<_, String>(1)));
                let from = checkpoint.as_ref().map_or(0, |(seq, _)| *seq) as i64;
                let rows = self
                    .client
                    .query(
                        "select entry from history where session_id = $1 and seq >= $2 order by seq",
                        &[&id, &from],
                    )
                    .await?;
                let entries = rows
                    .into_iter()
                    .map(|row| decode(row.get::<_, Value>(0)))
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| StoreError::Failed(format!("session {id}: {error}")))?;
                let counters = self
                    .client
                    .query_one(
                        "select coalesce(max(seq) + 1, 0), coalesce(max(message_id), 0) + 1 from history where session_id = $1",
                        &[&id],
                    )
                    .await?;
                Ok::<_, StoreError>(StoredSession {
                    id: SessionId(id),
                    key: key.to_owned(),
                    checkpoint,
                    entries,
                    next_seq: counters.get::<_, i64>(0) as u64,
                    next_message_id: counters.get::<_, i64>(1) as u64,
                    updated_at,
                })
            }
            .await;
            if loaded.is_err() {
                let _ = self.close(SessionId(id)).await;
            }
            loaded.map(Some)
        })
    }

    fn append<'a>(
        &'a self,
        session: SessionId,
        seq: u64,
        entry: &'a HistoryEntry,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            let message = message_id(entry).map(|id| id as i64);
            self.client
                .execute(
                    "insert into history (session_id, seq, kind, message_id, entry, text) values ($1, $2, $3, $4, $5, $6)",
                    &[&session.0, &(seq as i64), &kind(entry), &message, &encode(entry), &entry.search_text()],
                )
                .await?;
            self.client
                .execute(
                    "update sessions set updated_at = now() where id = $1",
                    &[&session.0],
                )
                .await?;
            Ok(())
        })
    }

    fn save_checkpoint<'a>(
        &'a self,
        session: SessionId,
        first_kept_seq: u64,
        summary: &'a str,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.client
                .execute(
                    "insert into checkpoints (session_id, first_kept_seq, summary) values ($1, $2, $3)
                     on conflict (session_id, first_kept_seq) do update set summary = excluded.summary, created_at = now()",
                    &[&session.0, &(first_kept_seq as i64), &summary],
                )
                .await?;
            self.client
                .execute(
                    "update sessions set updated_at = now() where id = $1",
                    &[&session.0],
                )
                .await?;
            Ok(())
        })
    }

    fn close(&self, session: SessionId) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            self.client
                .execute("select pg_advisory_unlock($1)", &[&session.0])
                .await?;
            Ok(())
        })
    }

    fn add_pending<'a>(
        &'a self,
        session: SessionId,
        row: &'a PendingRow,
    ) -> BoxFuture<'a, Result<(), StoreError>> {
        Box::pin(async move {
            self.client
                .execute(
                    "insert into pending (session_id, message_id, submitted_at, text) values ($1, $2, $3, $4)
                     on conflict (session_id, message_id) do update set submitted_at = excluded.submitted_at, text = excluded.text",
                    &[&session.0, &(row.message_id as i64), &row.submitted_at, &row.text],
                )
                .await?;
            Ok(())
        })
    }

    fn remove_pending(
        &self,
        session: SessionId,
        message_id: u64,
    ) -> BoxFuture<'_, Result<(), StoreError>> {
        Box::pin(async move {
            self.client
                .execute(
                    "delete from pending where session_id = $1 and message_id = $2",
                    &[&session.0, &(message_id as i64)],
                )
                .await?;
            Ok(())
        })
    }

    fn take_pending(
        &self,
        session: SessionId,
    ) -> BoxFuture<'_, Result<Vec<PendingRow>, StoreError>> {
        Box::pin(async move {
            let rows = self
                .client
                .query(
                    "delete from pending where session_id = $1 returning message_id, submitted_at, text",
                    &[&session.0],
                )
                .await?;
            let mut rows: Vec<PendingRow> = rows
                .iter()
                .map(|row| PendingRow {
                    message_id: row.get::<_, i64>(0) as u64,
                    submitted_at: row.get(1),
                    text: row.get(2),
                })
                .collect();
            rows.sort_by_key(|row| row.message_id);
            Ok(rows)
        })
    }

    fn recall_words<'a>(
        &'a self,
        scope: &'a RecallScope,
        current: SessionId,
        terms: &'a [String],
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let patterns: Vec<String> = terms.iter().map(|term| like_pattern(term)).collect();
            let rows = self
                .client
                .query(
                    &format!(
                        "{HIT_COLUMNS} where {IN_SCOPE} and {NOT_VISIBLE}
                         and coalesce(h.text, '') ilike all($3::text[])
                         order by h.created_at desc, h.session_id desc, h.seq desc limit $4"
                    ),
                    &[&scope.only_key(), &current.0, &patterns, &(limit as i64)],
                )
                .await
                .map_err(text_error)?;
            Ok(rows.iter().map(hit_row).collect())
        })
    }

    fn recall_missing<'a>(
        &'a self,
        scope: &'a RecallScope,
        model: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<(EntryRef, String)>, String>> {
        Box::pin(async move {
            let rows = self
                .client
                .query(
                    &format!(
                        "select h.session_id, h.seq, h.text from history h join sessions s on s.id = h.session_id
                         where {IN_SCOPE} and coalesce(h.text, '') <> ''
                         and not exists (select 1 from history_embeddings e
                             where e.session_id = h.session_id and e.seq = h.seq and e.model = $2)
                         order by h.session_id, h.seq limit $3"
                    ),
                    &[&scope.only_key(), &model, &(limit as i64)],
                )
                .await
                .map_err(text_error)?;
            Ok(rows
                .iter()
                .map(|row| {
                    (
                        EntryRef {
                            session_id: row.get(0),
                            seq: row.get(1),
                        },
                        row.get(2),
                    )
                })
                .collect())
        })
    }

    fn recall_put<'a>(
        &'a self,
        model: &'a str,
        vectors: &'a [(EntryRef, Vec<f32>)],
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            let insert = self
                .client
                .prepare(
                    "insert into history_embeddings (session_id, seq, model, embedding)
                     values ($1, $2, $3, $4::text::vector) on conflict do nothing",
                )
                .await
                .map_err(text_error)?;
            for (entry, vector) in vectors {
                self.client
                    .execute(
                        &insert,
                        &[&entry.session_id, &entry.seq, &model, &vector_text(vector)],
                    )
                    .await
                    .map_err(text_error)?;
            }
            Ok(())
        })
    }

    fn recall_nearest<'a>(
        &'a self,
        scope: &'a RecallScope,
        current: SessionId,
        model: &'a str,
        vector: &'a [f32],
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<Hit>, String>> {
        Box::pin(async move {
            let rows = self
                .client
                .query(
                    &format!(
                        "{HIT_COLUMNS} join history_embeddings e on e.session_id = h.session_id and e.seq = h.seq
                         where {IN_SCOPE} and {NOT_VISIBLE} and e.model = $3
                         order by e.embedding <=> $4::text::vector limit $5"
                    ),
                    &[
                        &scope.only_key(),
                        &current.0,
                        &model,
                        &vector_text(vector),
                        &(limit as i64),
                    ],
                )
                .await
                .map_err(text_error)?;
            Ok(rows.iter().map(hit_row).collect())
        })
    }

    fn recall_around<'a>(
        &'a self,
        scope: &'a RecallScope,
        entry: EntryRef,
        before: u64,
        after: u64,
    ) -> BoxFuture<'a, Result<Option<Vec<Hit>>, String>> {
        Box::pin(async move {
            let found = self
                .client
                .query_opt(
                    &format!(
                        "select 1 from history h join sessions s on s.id = h.session_id
                         where {IN_SCOPE} and h.session_id = $2 and h.seq = $3"
                    ),
                    &[&scope.only_key(), &entry.session_id, &entry.seq],
                )
                .await
                .map_err(text_error)?;
            if found.is_none() {
                return Ok(None);
            }
            let rows = self
                .client
                .query(
                    &format!(
                        "{HIT_COLUMNS} where h.session_id = $1 and h.seq between $2 and $3 order by h.seq"
                    ),
                    &[
                        &entry.session_id,
                        &(entry.seq - before as i64),
                        &(entry.seq + after as i64),
                    ],
                )
                .await
                .map_err(text_error)?;
            Ok(Some(rows.iter().map(hit_row).collect()))
        })
    }
}

/// The columns `hit_row` reads, from `history h` joined with `sessions s`.
const HIT_COLUMNS: &str =
    "select h.session_id, h.seq, s.key, s.created_at, h.created_at, h.kind, coalesce(h.text, '')
     from history h join sessions s on s.id = h.session_id";
/// The scope, with the one key or null for the owner's in `$1`.
const IN_SCOPE: &str =
    "(($1::text is null and (s.key like 'owner:%' or s.key like 'group:%')) or s.key = $1::text)";
/// Leaves out the entries the session in `$2` still sees.
const NOT_VISIBLE: &str = "not (h.session_id = $2 and h.seq >= coalesce(
         (select max(c.first_kept_seq) from checkpoints c where c.session_id = $2), 0))";

fn hit_row(row: &Row) -> Hit {
    Hit {
        entry: EntryRef {
            session_id: row.get(0),
            seq: row.get(1),
        },
        key: row.get(2),
        session_started: row.get(3),
        at: row.get(4),
        kind: row.get(5),
        text: row.get(6),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::agent_state::{
        AssistantEntry, AssistantPart, MediaRef, MessageId, MessageOrigin, Replay, ToolCall, UserEntry,
        UserPart,
    };

    fn sample() -> Vec<HistoryEntry> {
        vec![
            HistoryEntry::User(UserEntry {
                message_id: MessageId(7),
                origin: MessageOrigin::Owner,
                parts: vec![
                    UserPart::Text("[clock]\nhello".into()),
                    UserPart::Image(MediaRef {
                        sha256: "ab".into(),
                        mime: "image/jpeg".into(),
                        width: 2,
                        height: 1,
                    }),
                ],
            }),
            HistoryEntry::Assistant(AssistantEntry {
                parts: vec![
                    AssistantPart::Replay(Replay {
                        route: "responses".into(),
                        data: json!({"type": "reasoning", "encrypted_content": "x"}),
                    }),
                    AssistantPart::Text("Reading.".into()),
                    AssistantPart::ToolCall(ToolCall {
                        id: "a".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "a.txt"}),
                    }),
                    AssistantPart::ToolCall(ToolCall {
                        id: "b".into(),
                        name: "read_file".into(),
                        arguments: json!({"path": "b.txt"}),
                    }),
                ],
            }),
            HistoryEntry::ToolResult(ToolResultEntry {
                call_id: "a".into(),
                outcome: ToolOutcome::Success("alpha".into()),
                images: Vec::new(),
            }),
        ]
    }

    #[test]
    fn entries_round_trip_with_their_version() {
        for entry in sample()
            .into_iter()
            .chain([HistoryEntry::ToolResult(ToolResultEntry {
                call_id: "c".into(),
                outcome: ToolOutcome::Cancelled,
                images: Vec::new(),
            })])
        {
            let value = encode(&entry);
            assert_eq!(value["v"], 2);
            assert_eq!(decode(value).unwrap(), entry);
        }
        // A result with images keeps them; one stored before has none.
        let with_images = HistoryEntry::ToolResult(ToolResultEntry {
            call_id: "d".into(),
            outcome: ToolOutcome::Success("a.png".into()),
            images: vec![MediaRef {
                sha256: "cd".into(),
                mime: "image/png".into(),
                width: 3,
                height: 4,
            }],
        });
        assert_eq!(decode(encode(&with_images)).unwrap(), with_images);
        assert!(
            encode(&sample()[2]).get("images").is_none(),
            "stored as before"
        );
        let old = json!({"v": 2, "type": "tool_result", "call_id": "e", "outcome": {"type": "success", "value": "x"}});
        let HistoryEntry::ToolResult(old) = decode(old).unwrap() else {
            panic!("a tool result");
        };
        assert!(old.images.is_empty());
        let mut future = encode(&sample()[0]);
        future["v"] = 3.into();
        assert!(decode(future).unwrap_err().contains("format 3"));
        // A version 1 user entry reads as one text part.
        let old = json!({"v": 1, "type": "user", "message_id": 3, "origin": "owner", "text": "[clock]\nhi"});
        assert_eq!(
            decode(old).unwrap(),
            HistoryEntry::User(UserEntry::text_only(
                MessageId(3),
                MessageOrigin::Owner,
                "[clock]\nhi"
            ))
        );
    }

    #[tokio::test]
    async fn repair_closes_the_interrupted_calls() {
        let store = MemoryStore::new();
        let session = store.create("k", Path::new("/")).await.unwrap();
        for (seq, entry) in sample().iter().enumerate() {
            store.append(session.id, seq as u64, entry).await.unwrap();
        }
        store.close(session.id).await.unwrap();
        let mut loaded = store.load_current("k").await.unwrap().unwrap();
        assert_eq!(loaded.next_message_id, 8);
        assert!(repair(store.as_ref(), &mut loaded).await.unwrap());
        let HistoryEntry::ToolResult(added) = loaded.entries.last().unwrap() else {
            panic!("a result was added");
        };
        assert_eq!(added.call_id.0, "b");
        assert_eq!(added.outcome, ToolOutcome::Error(INTERRUPTED_CALL.into()));
        assert_eq!(store.seqs(loaded.id), [0, 1, 2, 3]);
        // A finished session needs nothing.
        assert!(!repair(store.as_ref(), &mut loaded).await.unwrap());
    }

    #[tokio::test]
    async fn a_second_load_is_busy_until_closed() {
        let store = MemoryStore::new();
        let session = store.create("k", Path::new("/")).await.unwrap();
        assert_eq!(store.load_current("k").await.unwrap_err(), StoreError::Busy);
        store.close(session.id).await.unwrap();
        assert!(store.load_current("k").await.unwrap().is_some());
        assert!(store.load_current("other").await.unwrap().is_none());
    }
}
