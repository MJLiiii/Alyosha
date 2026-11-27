//! Memory: curated facts kept across sessions in one global store in the
//! session database.
//!
//! The model writes them freely through the `memory` tool and each write shows
//! in the status area. A version check keeps two sessions from overwriting
//! each other. The index goes into the system instruction once per session.
//! See `docs/Memory.md#implementation`.
//!
//! `MemoryTables` is the storage: `PgStore` keeps the rows in PostgreSQL, and
//! `InMemoryTables` keeps them in memory for the scripted tests.

use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, PoisonError},
};

use chrono::NaiveDate;
use serde_json::{Value, json};

use crate::{
    BoxFuture,
    tools::{RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler},
};

pub const DESCRIPTION_LIMIT: usize = 160;
pub const BODY_LIMIT: usize = 4 * 1024;
pub const INDEX_BUDGET: usize = 4 * 1024;

/// One row of `memories`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryRow {
    pub name: String,
    pub description: String,
    pub body: String,
    pub updated: NaiveDate,
    pub version: i64,
}

/// One row of `user_notes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotesRow {
    pub person: String,
    pub name: String,
    pub notes: String,
    pub updated: NaiveDate,
    pub version: i64,
}

/// The `memories` and `user_notes` tables. Each write is one statement
/// guarded by a version.
pub trait MemoryTables: Send + Sync {
    /// Every memory, in no particular order.
    fn memories(&self) -> BoxFuture<'_, Result<Vec<MemoryRow>, String>>;
    fn memory<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<MemoryRow>, String>>;
    /// Inserts the row at version 1 unless the name exists. Returns whether
    /// it did.
    fn insert_memory<'a>(&'a self, row: &'a MemoryRow) -> BoxFuture<'a, Result<bool, String>>;
    /// Replaces the row's description, body, and date if its version is
    /// `seen`, and returns the new version.
    fn update_memory<'a>(
        &'a self,
        row: &'a MemoryRow,
        seen: i64,
    ) -> BoxFuture<'a, Result<Option<i64>, String>>;
    /// Deletes the row if its version is `seen`. Returns whether it did.
    fn delete_memory<'a>(&'a self, name: &'a str, seen: i64)
    -> BoxFuture<'a, Result<bool, String>>;
    fn notes<'a>(&'a self, person: &'a str) -> BoxFuture<'a, Result<Option<NotesRow>, String>>;
    /// Inserts the row at version 1 unless the person has notes.
    fn insert_notes<'a>(&'a self, row: &'a NotesRow) -> BoxFuture<'a, Result<bool, String>>;
    /// Replaces the name, notes, and date if the version is `seen`, and
    /// returns the new version.
    fn update_notes<'a>(
        &'a self,
        row: &'a NotesRow,
        seen: i64,
    ) -> BoxFuture<'a, Result<Option<i64>, String>>;
}

/// The tables in memory, for tests.
#[derive(Default)]
pub struct InMemoryTables {
    inner: Mutex<InMemoryInner>,
}

#[derive(Default)]
struct InMemoryInner {
    memories: BTreeMap<String, MemoryRow>,
    notes: BTreeMap<String, NotesRow>,
}

impl InMemoryTables {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, InMemoryInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts a row in directly, as another process or a test setup would.
    pub fn put_memory(&self, row: MemoryRow) {
        self.lock().memories.insert(row.name.clone(), row);
    }

    /// Puts a person's notes in directly.
    pub fn put_notes(&self, row: NotesRow) {
        self.lock().notes.insert(row.person.clone(), row);
    }

    /// Every person's notes, for checks.
    pub fn all_notes(&self) -> Vec<NotesRow> {
        self.lock().notes.values().cloned().collect()
    }
}

impl MemoryTables for InMemoryTables {
    fn memories(&self) -> BoxFuture<'_, Result<Vec<MemoryRow>, String>> {
        Box::pin(async move { Ok(self.lock().memories.values().cloned().collect()) })
    }

    fn memory<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<Option<MemoryRow>, String>> {
        Box::pin(async move { Ok(self.lock().memories.get(name).cloned()) })
    }

    fn insert_memory<'a>(&'a self, row: &'a MemoryRow) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut inner = self.lock();
            if inner.memories.contains_key(&row.name) {
                return Ok(false);
            }
            let mut row = row.clone();
            row.version = 1;
            inner.memories.insert(row.name.clone(), row);
            Ok(true)
        })
    }

    fn update_memory<'a>(
        &'a self,
        row: &'a MemoryRow,
        seen: i64,
    ) -> BoxFuture<'a, Result<Option<i64>, String>> {
        Box::pin(async move {
            let mut inner = self.lock();
            match inner.memories.get_mut(&row.name) {
                Some(stored) if stored.version == seen => {
                    stored.description = row.description.clone();
                    stored.body = row.body.clone();
                    stored.updated = row.updated;
                    stored.version += 1;
                    Ok(Some(stored.version))
                }
                _ => Ok(None),
            }
        })
    }

    fn delete_memory<'a>(
        &'a self,
        name: &'a str,
        seen: i64,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut inner = self.lock();
            if inner
                .memories
                .get(name)
                .is_some_and(|row| row.version == seen)
            {
                inner.memories.remove(name);
                return Ok(true);
            }
            Ok(false)
        })
    }

    fn notes<'a>(&'a self, person: &'a str) -> BoxFuture<'a, Result<Option<NotesRow>, String>> {
        Box::pin(async move { Ok(self.lock().notes.get(person).cloned()) })
    }

    fn insert_notes<'a>(&'a self, row: &'a NotesRow) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(async move {
            let mut inner = self.lock();
            if inner.notes.contains_key(&row.person) {
                return Ok(false);
            }
            let mut row = row.clone();
            row.version = 1;
            inner.notes.insert(row.person.clone(), row);
            Ok(true)
        })
    }

    fn update_notes<'a>(
        &'a self,
        row: &'a NotesRow,
        seen: i64,
    ) -> BoxFuture<'a, Result<Option<i64>, String>> {
        Box::pin(async move {
            let mut inner = self.lock();
            match inner.notes.get_mut(&row.person) {
                Some(stored) if stored.version == seen => {
                    stored.name = row.name.clone();
                    stored.notes = row.notes.clone();
                    stored.updated = row.updated;
                    stored.version += 1;
                    Ok(Some(stored.version))
                }
                _ => Ok(None),
            }
        })
    }
}

/// One memory as the index and `list` show it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub name: String,
    pub description: String,
    pub updated: NaiveDate,
}

impl From<&MemoryRow> for MemoryEntry {
    fn from(row: &MemoryRow) -> Self {
        Self {
            name: row.name.clone(),
            description: row.description.clone(),
            updated: row.updated,
        }
    }
}

/// The index for the system instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndex {
    pub text: String,
    pub omitted: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Created,
    Updated,
}

/// Where today's date comes from, in the owner's timezone.
pub type Today = Arc<dyn Fn() -> NaiveDate + Send + Sync>;

/// Whether a name is valid: `[a-z0-9][a-z0-9-]{0,63}`.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

fn storage_error(error: String) -> ToolError {
    ToolError::failed(format!("The memory store failed: {error}"))
}

/// The entries, newest first, then by name.
fn sorted_entries(rows: &[MemoryRow]) -> Vec<MemoryEntry> {
    let mut entries: Vec<MemoryEntry> = rows.iter().map(MemoryEntry::from).collect();
    entries.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.name.cmp(&b.name)));
    entries
}

/// The index of the memories in `tables`.
pub async fn index(tables: &dyn MemoryTables) -> Result<MemoryIndex, String> {
    let entries = sorted_entries(&tables.memories().await?);
    let (text, omitted) = index_text(&entries);
    Ok(MemoryIndex { text, omitted })
}

/// What `alyosha memory list` prints: one line per memory, sorted by name,
/// then the counts.
pub fn host_list(rows: &[MemoryRow], people: i64) -> String {
    let mut rows: Vec<&MemoryRow> = rows.iter().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let mut text: String = rows
        .iter()
        .map(|row| format!("{} ({}): {}\n", row.name, row.updated, row.description))
        .collect();
    let memories = if rows.len() == 1 {
        "memory"
    } else {
        "memories"
    };
    let people_word = if people == 1 { "person" } else { "people" };
    text.push_str(&format!(
        "{} {memories} · {people} {people_word} with notes\n",
        rows.len()
    ));
    text
}

/// A session's view of the memory store: the tables and the version of each
/// memory this session read or wrote.
pub struct MemoryStore {
    tables: Arc<dyn MemoryTables>,
    today: Today,
    seen: Mutex<HashMap<String, i64>>,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore").finish_non_exhaustive()
    }
}

impl MemoryStore {
    pub fn open(tables: Arc<dyn MemoryTables>, today: Today) -> Self {
        Self {
            tables,
            today,
            seen: Mutex::default(),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, HashMap<String, i64>> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub async fn index(&self) -> Result<MemoryIndex, String> {
        index(self.tables.as_ref()).await
    }

    pub async fn list(&self) -> Result<Vec<MemoryEntry>, ToolError> {
        let rows = self.tables.memories().await.map_err(storage_error)?;
        Ok(sorted_entries(&rows))
    }

    /// The memory with its header, and records its version for this
    /// session.
    pub async fn read(&self, name: &str) -> Result<String, ToolError> {
        check_name(name)?;
        let row = self
            .tables
            .memory(name)
            .await
            .map_err(storage_error)?
            .ok_or_else(|| missing(name))?;
        self.seen().insert(name.to_owned(), row.version);
        Ok(format!(
            "---\ndescription: {}\nupdated: {}\n---\n{}",
            row.description, row.updated, row.body
        ))
    }

    pub async fn write(
        &self,
        name: &str,
        description: &str,
        content: &str,
    ) -> Result<WriteKind, ToolError> {
        check_name(name)?;
        if description.trim().is_empty()
            || description.contains('\n')
            || description.len() > DESCRIPTION_LIMIT
        {
            return Err(ToolError::failed(format!(
                "A description is one nonempty line of at most {DESCRIPTION_LIMIT} bytes."
            )));
        }
        if content.len() > BODY_LIMIT {
            return Err(ToolError::failed(format!(
                "A memory holds at most {BODY_LIMIT} bytes. Shorten it or split it."
            )));
        }
        if let Some(pattern) =
            credential_pattern(description).or_else(|| credential_pattern(content))
        {
            return Err(ToolError::failed(format!(
                "This looks like a secret ({pattern}). Memories must not hold secrets."
            )));
        }
        let today = (self.today)();
        let entries = sorted_entries(&self.tables.memories().await.map_err(storage_error)?);
        let before = full_index_len(&entries);
        let mut after_entries: Vec<MemoryEntry> = entries
            .iter()
            .filter(|entry| entry.name != name)
            .cloned()
            .collect();
        after_entries.push(MemoryEntry {
            name: name.to_owned(),
            description: description.to_owned(),
            updated: today,
        });
        let after = full_index_len(&after_entries);
        if after > INDEX_BUDGET && after > before {
            return Err(ToolError::failed(
                "The memory index is full. Merge related memories or delete ones that no longer apply, then try again.",
            ));
        }
        let row = MemoryRow {
            name: name.to_owned(),
            description: description.to_owned(),
            body: content.to_owned(),
            updated: today,
            version: 1,
        };
        let seen = self.seen().get(name).copied();
        let (kind, version) = match seen {
            Some(seen) => match self
                .tables
                .update_memory(&row, seen)
                .await
                .map_err(storage_error)?
            {
                Some(version) => (WriteKind::Updated, version),
                None => return Err(changed(name)),
            },
            None => {
                if self
                    .tables
                    .insert_memory(&row)
                    .await
                    .map_err(storage_error)?
                {
                    (WriteKind::Created, 1)
                } else {
                    return Err(changed(name));
                }
            }
        };
        self.seen().insert(name.to_owned(), version);
        Ok(kind)
    }

    pub async fn delete(&self, name: &str) -> Result<(), ToolError> {
        check_name(name)?;
        let seen = self.seen().get(name).copied();
        if let Some(seen) = seen
            && self
                .tables
                .delete_memory(name, seen)
                .await
                .map_err(storage_error)?
        {
            self.seen().remove(name);
            return Ok(());
        }
        match self.tables.memory(name).await.map_err(storage_error)? {
            Some(_) => Err(changed(name)),
            None => Err(missing(name)),
        }
    }
}

fn missing(name: &str) -> ToolError {
    ToolError::failed(format!("There is no memory named {name}."))
}

fn changed(name: &str) -> ToolError {
    ToolError::failed(format!(
        "{name} changed since you read it, or was never read in this session. Read it again first."
    ))
}

fn check_name(name: &str) -> Result<(), ToolError> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(ToolError::failed(format!(
            "{name:?} is not a valid memory name: use lowercase letters, digits, and hyphens, up to 64 characters."
        )))
    }
}

fn line(entry: &MemoryEntry) -> String {
    format!(
        "- {} ({}): {}\n",
        entry.name, entry.updated, entry.description
    )
}

fn full_index_len(entries: &[MemoryEntry]) -> usize {
    entries.iter().map(|entry| line(entry).len()).sum()
}

/// Lines while they fit the budget, then the omitted count.
fn index_text(entries: &[MemoryEntry]) -> (String, usize) {
    let mut text = String::new();
    let mut shown = 0;
    for entry in entries {
        let next = line(entry);
        if text.len() + next.len() > INDEX_BUDGET {
            break;
        }
        text.push_str(&next);
        shown += 1;
    }
    let omitted = entries.len() - shown;
    if omitted > 0 {
        text.push_str(&format!(
            "{omitted} more memories are not shown. Use memory list.\n"
        ));
    }
    (text, omitted)
}

/// The first credential pattern found in `text`, by its kind.
pub(crate) fn credential_pattern(text: &str) -> Option<&'static str> {
    if text.contains("-----BEGIN ") && text.contains("PRIVATE KEY-----") {
        return Some("a private key");
    }
    let token_chars = |s: &str, at: usize, min: usize, allowed: fn(char) -> bool| {
        s[at..].chars().take_while(|c| allowed(*c)).count() >= min
    };
    for (index, _) in text.match_indices("sk-") {
        if token_chars(text, index + 3, 20, |c| {
            c.is_ascii_alphanumeric() || c == '-' || c == '_'
        }) {
            return Some("an API key");
        }
    }
    for prefix in ["ghp_", "gho_", "github_pat_"] {
        for (index, _) in text.match_indices(prefix) {
            if token_chars(text, index + prefix.len(), 10, |c| {
                c.is_ascii_alphanumeric() || c == '_'
            }) {
                return Some("a GitHub token");
            }
        }
    }
    for (index, _) in text.match_indices("AKIA") {
        if token_chars(text, index + 4, 16, |c| {
            c.is_ascii_uppercase() || c.is_ascii_digit()
        }) {
            return Some("an AWS key");
        }
    }
    for prefix in ["xoxb-", "xoxp-"] {
        for (index, _) in text.match_indices(prefix) {
            if token_chars(text, index + prefix.len(), 10, |c| {
                c.is_ascii_alphanumeric() || c == '-'
            }) {
                return Some("a Slack token");
            }
        }
    }
    for (index, _) in text.match_indices("AIza") {
        if token_chars(text, index + 4, 35, |c| {
            c.is_ascii_alphanumeric() || c == '-' || c == '_'
        }) {
            return Some("a Google API key");
        }
    }
    None
}

/// The memory section of the normal system instruction.
pub fn instruction_section(index: &MemoryIndex) -> String {
    let lines = if index.text.is_empty() {
        "No memories yet.\n".to_owned()
    } else {
        index.text.clone()
    };
    format!(
        "## Memory\nYou have a long-term memory shared across sessions. Use the memory tool to read a\nmemory before relying on it. Save lasting facts: the owner's preferences, facts about\nthe owner, corrections, and stable facts about their machines and projects. Update an\nexisting memory rather than adding a near-duplicate. Do not save facts the workspace\nalready records, details of the current task only, or secrets. Each memory is true as\nof its date; verify it before acting on it.\n\n{lines}"
    )
}

/// The built-in `memory` tool.
pub fn memory_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "memory".into(),
            description: "Your long-term memory, shared across sessions. `list` shows every memory, `read` returns one, `write` creates or replaces a whole memory from name, description (one line), and content, and `delete` removes one. To change or delete an existing memory, `read` it first in this session. Names use lowercase letters, digits, and hyphens.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "enum": ["list", "read", "write", "delete"]},
                    "name": {"type": "string"},
                    "description": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            if context.scope != Scope::Direct {
                return Err(ToolError::failed("memory is for direct calls only."));
            }
            let store = context
                .session
                .memory
                .as_ref()
                .ok_or_else(|| ToolError::failed("This session has no memory store."))?;
            let name = arguments["name"].as_str();
            let need_name = || name.ok_or_else(|| ToolError::failed("This command needs a name."));
            match arguments["command"].as_str().unwrap_or_default() {
                "list" => {
                    let entries = store.list().await?;
                    if entries.is_empty() {
                        return Ok(ToolOutput::text("No memories yet."));
                    }
                    Ok(ToolOutput::text(entries.iter().map(line).collect::<String>()))
                }
                "read" => Ok(ToolOutput::text(store.read(need_name()?).await?)),
                "write" => {
                    let name = need_name()?;
                    let description = arguments["description"]
                        .as_str()
                        .ok_or_else(|| ToolError::failed("write needs a description."))?;
                    let content = arguments["content"]
                        .as_str()
                        .ok_or_else(|| ToolError::failed("write needs content."))?;
                    let kind = store.write(name, description, content).await?;
                    let text = if kind == WriteKind::Created { "created" } else { "updated" };
                    Ok(ToolOutput::text(text).with_notice(format!("Remembered: {name}")))
                }
                "delete" => {
                    let name = need_name()?;
                    store.delete(name).await?;
                    Ok(ToolOutput::text(format!("Deleted {name}.")).with_notice(format!("Forgot: {name}")))
                }
                other => Err(ToolError::failed(format!("Unknown memory command {other:?}."))),
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        text.parse().unwrap()
    }

    fn store(tables: &Arc<InMemoryTables>, day: &str) -> MemoryStore {
        let day = date(day);
        MemoryStore::open(tables.clone(), Arc::new(move || day))
    }

    #[test]
    fn the_host_list_is_sorted_by_name_and_counts() {
        let row = |name: &str, day: &str| MemoryRow {
            name: name.into(),
            description: format!("About {name}"),
            body: String::new(),
            updated: date(day),
            version: 1,
        };
        assert_eq!(
            host_list(
                &[
                    row("thesis-repo", "2026-11-26"),
                    row("editor", "2026-11-20")
                ],
                2
            ),
            "editor (2026-11-20): About editor\nthesis-repo (2026-11-26): About thesis-repo\n2 memories · 2 people with notes\n"
        );
        assert_eq!(
            host_list(&[row("a", "2026-11-20")], 1).lines().last(),
            Some("1 memory · 1 person with notes")
        );
        assert_eq!(host_list(&[], 0), "0 memories · 0 people with notes\n");
    }

    #[tokio::test]
    async fn an_empty_store_then_a_first_write() {
        let tables = InMemoryTables::new();
        let store = store(&tables, "2026-11-25");
        assert_eq!(
            instruction_section(&store.index().await.unwrap())
                .lines()
                .last(),
            Some("No memories yet.")
        );
        assert_eq!(
            store
                .write(
                    "thesis-repo",
                    "Thesis lives in ~/Projects/thesis",
                    "Use LaTeX."
                )
                .await
                .unwrap(),
            WriteKind::Created
        );
        assert_eq!(
            store.read("thesis-repo").await.unwrap(),
            "---\ndescription: Thesis lives in ~/Projects/thesis\nupdated: 2026-11-25\n---\nUse LaTeX."
        );
    }

    #[tokio::test]
    async fn an_update_takes_the_new_date() {
        let tables = InMemoryTables::new();
        store(&tables, "2026-11-20")
            .write("editor", "Uses Helix", "x")
            .await
            .unwrap();
        let later = store(&tables, "2026-11-25");
        later.read("editor").await.unwrap();
        assert_eq!(
            later
                .write("editor", "Uses Helix and Zed", "y")
                .await
                .unwrap(),
            WriteKind::Updated
        );
        let entries = later.list().await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].updated.to_string(), "2026-11-25");
        assert_eq!(
            later.index().await.unwrap().text,
            "- editor (2026-11-25): Uses Helix and Zed\n"
        );
    }

    #[tokio::test]
    async fn a_change_from_another_session_must_be_read_first() {
        let tables = InMemoryTables::new();
        let first = store(&tables, "2026-11-25");
        first.write("thesis-repo", "Where", "A").await.unwrap();
        let a = store(&tables, "2026-11-25");
        let b = store(&tables, "2026-11-25");
        a.read("thesis-repo").await.unwrap();
        b.read("thesis-repo").await.unwrap();
        b.write("thesis-repo", "Where", "B").await.unwrap();

        let error = a.write("thesis-repo", "Where", "A2").await.unwrap_err();
        assert!(
            error.to_string().contains(
                "thesis-repo changed since you read it, or was never read in this session"
            ),
            "{error}"
        );
        assert!(a.delete("thesis-repo").await.is_err());
        assert!(b.read("thesis-repo").await.unwrap().ends_with("\nB"));

        a.read("thesis-repo").await.unwrap();
        a.write("thesis-repo", "Where", "A2").await.unwrap();
        assert!(a.read("thesis-repo").await.unwrap().ends_with("\nA2"));

        // Writing an existing memory without reading it first fails.
        let fresh = store(&tables, "2026-11-25");
        assert!(fresh.write("thesis-repo", "Where", "C").await.is_err());
        // Deleting needs a read too; a missing memory says so.
        assert!(
            fresh
                .delete("thesis-repo")
                .await
                .unwrap_err()
                .to_string()
                .contains("changed since")
        );
        assert!(
            fresh
                .delete("nothing")
                .await
                .unwrap_err()
                .to_string()
                .contains("There is no memory named nothing")
        );
        a.delete("thesis-repo").await.unwrap();
        assert!(a.read("thesis-repo").await.is_err());
    }

    #[tokio::test]
    async fn bad_names_are_refused() {
        let tables = InMemoryTables::new();
        let store = store(&tables, "2026-11-25");
        for name in ["../x", "A", &"a".repeat(65), ""] {
            assert!(store.write(name, "d", "c").await.is_err(), "{name}");
            assert!(store.read(name).await.is_err(), "{name}");
        }
        assert!(tables.memories().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn oversized_and_secret_text_is_refused() {
        let tables = InMemoryTables::new();
        let store = store(&tables, "2026-11-25");
        store.write("keep", "original", "body").await.unwrap();
        let before = store.read("keep").await.unwrap();
        for (description, body) in [
            ("d".repeat(161).as_str(), "body"),
            ("two\nlines", "body"),
            ("fine", "b".repeat(4097).as_str()),
            ("fine", "key sk-abcdefghijklmnopqrstuvwxyz"),
            ("fine", "-----BEGIN OPENSSH PRIVATE KEY-----"),
            ("fine", "AKIAABCDEFGHIJKLMNOP"),
        ] {
            assert!(store.write("keep", description, body).await.is_err());
        }
        assert_eq!(store.read("keep").await.unwrap(), before);
    }

    #[tokio::test]
    async fn a_full_index_refuses_growth_but_allows_same_size_updates() {
        let tables = InMemoryTables::new();
        let store = store(&tables, "2026-11-25");
        let description = "d".repeat(150);
        let mut written = 0;
        while store
            .write(&format!("m{written:03}"), &description, "x")
            .await
            .is_ok()
        {
            written += 1;
        }
        assert!(written > 10);
        let error = store
            .write("one-more", &description, "x")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Merge"), "{error}");
        store.write("m000", &description, "changed").await.unwrap();

        // Rows inserted directly beyond the budget: newest first, then the
        // count.
        for extra in 0..10 {
            tables.put_memory(MemoryRow {
                name: format!("old{extra}"),
                description: description.clone(),
                body: "x".into(),
                updated: date("2020-01-01"),
                version: 1,
            });
        }
        let index = store.index().await.unwrap();
        assert!(index.omitted >= 10);
        assert!(index.text.starts_with("- m000 (2026-11-25)"));
        assert!(index.text.ends_with(&format!(
            "{} more memories are not shown. Use memory list.\n",
            index.omitted
        )));
        assert!(index.text.len() <= INDEX_BUDGET + 100);
    }
}
