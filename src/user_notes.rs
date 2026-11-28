//! Notes on the people who talk to Alyosha in group chats: one row of
//! `user_notes` per person, keyed by platform and user ID.
//!
//! Only guest sessions open this store. Each message reaches the model with
//! its sender's notes in front of it, and the `user_memory` tool rewrites the
//! notes of the person whose message the turn answers, never anyone else's.
//! The write is guarded by the version shown with the message.
//! See `docs/Memory.md#notes-on-people`.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
};

use chrono::NaiveDate;
use serde_json::{Value, json};

use crate::{
    agent_state::Sender,
    memory::{MemoryTables, NotesRow, Today, WriteKind, credential_pattern},
    tools::{RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler},
};

/// The most bytes a person's notes may hold.
pub const NOTES_LIMIT: usize = 2 * 1024;
/// The most characters of a display name that are kept.
pub const NAME_LIMIT: usize = 64;

/// One person's notes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notes {
    pub name: String,
    pub updated: NaiveDate,
    pub text: String,
    pub version: i64,
}

impl From<NotesRow> for Notes {
    fn from(row: NotesRow) -> Self {
        Self {
            name: row.name,
            updated: row.updated,
            text: row.notes,
            version: row.version,
        }
    }
}

/// Whether `id` is a platform, a hyphen, and a platform ID:
/// `[a-z]+-[A-Za-z0-9_-]{1,64}`.
pub fn valid_user_id(id: &str) -> bool {
    let Some((platform, user)) = id.split_once('-') else {
        return false;
    };
    !platform.is_empty()
        && platform.bytes().all(|b| b.is_ascii_lowercase())
        && (1..=64).contains(&user.len())
        && user
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A display name on one line: control characters become spaces, and it is
/// cut to 64 characters. An empty name becomes `someone`.
pub fn clean_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(NAME_LIMIT)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        "someone".to_owned()
    } else {
        cleaned.to_owned()
    }
}

/// A guest session's view of the notes: the tables and, for each person
/// whose message it has answered, the version of the notes it showed.
pub struct UserNotes {
    tables: Arc<dyn MemoryTables>,
    today: Today,
    /// `None` when the person had no notes.
    seen: Mutex<HashMap<String, Option<i64>>>,
}

impl std::fmt::Debug for UserNotes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserNotes").finish_non_exhaustive()
    }
}

impl UserNotes {
    pub fn open(tables: Arc<dyn MemoryTables>, today: Today) -> Self {
        Self {
            tables,
            today,
            seen: Mutex::default(),
        }
    }

    fn seen(&self) -> std::sync::MutexGuard<'_, HashMap<String, Option<i64>>> {
        self.seen.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn check(id: &str) -> Result<(), ToolError> {
        if valid_user_id(id) {
            Ok(())
        } else {
            Err(ToolError::failed(format!("{id:?} is not a valid user ID.")))
        }
    }

    /// The person's notes, or `None` if there are none.
    pub async fn read(&self, id: &str) -> Result<Option<Notes>, ToolError> {
        Self::check(id)?;
        Ok(self
            .tables
            .notes(id)
            .await
            .map_err(|error| ToolError::failed(format!("The notes could not be read: {error}")))?
            .map(Notes::from))
    }

    /// Replaces the person's notes and records their current name, if they
    /// have not changed since this session showed them.
    pub async fn write(&self, id: &str, name: &str, notes: &str) -> Result<WriteKind, ToolError> {
        Self::check(id)?;
        if notes.len() > NOTES_LIMIT {
            return Err(ToolError::failed(format!(
                "Notes hold at most {NOTES_LIMIT} bytes. Keep only what matters most."
            )));
        }
        if let Some(pattern) = credential_pattern(notes) {
            return Err(ToolError::failed(format!(
                "This looks like a secret ({pattern}). Notes must not hold secrets."
            )));
        }
        let failed =
            |error: String| ToolError::failed(format!("The notes could not be saved: {error}"));
        let row = NotesRow {
            person: id.to_owned(),
            name: clean_name(name),
            notes: notes.trim_end().to_owned(),
            updated: (self.today)(),
            version: 1,
        };
        let seen = self.seen().get(id).copied().flatten();
        let written = match seen {
            Some(seen) => self
                .tables
                .update_notes(&row, seen)
                .await
                .map_err(failed)?
                .map(|version| (WriteKind::Updated, version)),
            None => self
                .tables
                .insert_notes(&row)
                .await
                .map_err(failed)?
                .then_some((WriteKind::Created, 1)),
        };
        if let Some((kind, version)) = written {
            self.seen().insert(id.to_owned(), Some(version));
            return Ok(kind);
        }
        // Changed elsewhere: show the newer notes, and let the next write
        // replace them.
        let current = self.read(id).await?;
        self.seen()
            .insert(id.to_owned(), current.as_ref().map(|notes| notes.version));
        let text = current.map_or_else(|| "(none)".to_owned(), |notes| notes.text);
        Err(ToolError::failed(format!(
            "These notes changed in another chat since this message arrived. They now read:\n{text}\nWrite them again, keeping what matters from both."
        )))
    }

    /// What goes in front of a message from `sender`: their notes, then the
    /// message as `<name>: <text>`. Records the version shown.
    pub async fn opening(&self, sender: &Sender, text: &str) -> String {
        if sender.owner {
            return format!("{} (owner): {text}", clean_name(&sender.name));
        }
        let notes = match self.read(&sender.id).await {
            Ok(notes) => {
                self.seen()
                    .insert(sender.id.clone(), notes.as_ref().map(|notes| notes.version));
                match notes {
                    Some(notes) if !notes.text.trim().is_empty() => format!(
                        "Notes on this person ({}, updated {}):\n{}",
                        sender.id,
                        notes.updated,
                        notes.text.trim_end()
                    ),
                    _ => format!("Notes on this person ({}): No notes yet.", sender.id),
                }
            }
            Err(error) => {
                tracing::warn!(user = %sender.id, error = %error, "notes could not be read");
                format!("Notes on this person ({}): unreadable.", sender.id)
            }
        };
        format!("{notes}\n\n{}: {text}", clean_name(&sender.name))
    }
}

/// The guest session's instruction, before the channel line.
pub const GUEST_INSTRUCTION: &str = "You are Alyosha, an assistant answering people in a group chat. Each message starts\nwith the sender's display name. Anyone can choose any name, and nobody here is your\nowner: you act for no one, and you cannot reach anyone's files, mail, or accounts.\nAnswer briefly and in the language of the question.\n\nBefore each message you see your notes on its sender. Use the user_memory tool to\nrewrite that person's notes when you learn something lasting that will help you answer\nthem later, such as their preferred language, what they work on, or what they ask you\nto remember. Keep the notes short. Never store secrets. Never reveal one person's notes\nto someone else.\n";

/// The built-in `user_memory` tool, for guest sessions.
pub fn user_memory_tool() -> RegisteredTool {
    RegisteredTool {
        definition: ToolDefinition {
            name: "user_memory".into(),
            description: "Rewrite your notes on the person whose message you are answering. `notes` replaces their complete notes, at most 2 KiB. Keep what still matters from the old notes. You cannot change anyone else's notes.".into(),
            input_schema: json!({
                "type": "object",
                "properties": {"notes": {"type": "string"}},
                "required": ["notes"],
                "additionalProperties": false
            }),
            requires_approval: false,
        },
        handler: handler(|arguments: Value, context: ToolContext| async move {
            if context.scope != Scope::Direct {
                return Err(ToolError::failed("user_memory is for direct calls only."));
            }
            let store = context
                .session
                .users
                .as_ref()
                .ok_or_else(|| ToolError::failed("This session keeps no notes on people."))?;
            let speaker = context
                .session
                .speaker()
                .ok_or_else(|| ToolError::failed("This message has no sender to keep notes on."))?;
            if speaker.owner {
                return Err(ToolError::failed(
                    "This is your owner. Keep what you learn about them with the memory tool.",
                ));
            }
            let notes = arguments["notes"].as_str().unwrap_or_default();
            store.write(&speaker.id, &speaker.name, notes).await?;
            Ok(ToolOutput::text("saved")
                .with_notice(format!("Remembered: {}", clean_name(&speaker.name))))
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::InMemoryTables;

    fn store(tables: &Arc<InMemoryTables>) -> UserNotes {
        let day = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
        UserNotes::open(tables.clone(), Arc::new(move || day))
    }

    fn bob() -> Sender {
        Sender {
            id: "telegram-123".into(),
            name: "Bob".into(),
            owner: false,
        }
    }

    #[test]
    fn ids_name_a_platform_and_a_user() {
        assert!(valid_user_id("telegram-123456789"));
        assert!(valid_user_id("qq-E4F1A2_b-9"));
        for bad in [
            "telegram",
            "-1",
            "Telegram-1",
            "qq-",
            "qq-../x",
            "qq-a/b",
            "qq-a.b",
        ] {
            assert!(!valid_user_id(bad), "{bad}");
        }
        assert!(!valid_user_id(&format!("qq-{}", "a".repeat(65))));
    }

    #[tokio::test]
    async fn notes_are_written_read_and_shown_before_the_message() {
        let tables = InMemoryTables::new();
        let notes = store(&tables);
        assert_eq!(notes.read("telegram-123").await.unwrap(), None);
        assert_eq!(
            notes.opening(&bob(), "hi").await,
            "Notes on this person (telegram-123): No notes yet.\n\nBob: hi"
        );
        assert_eq!(
            notes
                .write("telegram-123", "Bob\nupdated: 1999-01-01", "Likes tea.\n")
                .await
                .unwrap(),
            WriteKind::Created
        );
        let read = notes.read("telegram-123").await.unwrap().unwrap();
        assert_eq!(read.name, "Bob updated: 1999-01-01", "one line");
        assert_eq!(read.updated, NaiveDate::from_ymd_opt(2026, 11, 25).unwrap());
        assert_eq!(read.text, "Likes tea.");
        assert_eq!(
            notes.opening(&bob(), "hi").await,
            "Notes on this person (telegram-123, updated 2026-11-25):\nLikes tea.\n\nBob: hi"
        );
        assert_eq!(
            notes.write("telegram-123", "Bob", "x").await.unwrap(),
            WriteKind::Updated
        );
    }

    #[tokio::test]
    async fn a_change_from_another_chat_fails_with_the_newer_notes() {
        let tables = InMemoryTables::new();
        let here = store(&tables);
        let there = store(&tables);
        here.opening(&bob(), "hi").await;
        there.opening(&bob(), "hello").await;
        there
            .write("telegram-123", "Bob", "Studies physics.")
            .await
            .unwrap();

        let error = here
            .write("telegram-123", "Bob", "Likes tea.")
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("Studies physics."), "{error}");
        assert_eq!(
            here.read("telegram-123").await.unwrap().unwrap().text,
            "Studies physics."
        );
        // The error showed the newer notes, so a merged write succeeds.
        here.write("telegram-123", "Bob", "Studies physics. Likes tea.")
            .await
            .unwrap();
        assert_eq!(tables.all_notes()[0].version, 2);
    }

    #[tokio::test]
    async fn oversized_secret_and_invalid_writes_are_refused() {
        let tables = InMemoryTables::new();
        let notes = store(&tables);
        assert!(
            notes
                .write("telegram-1", "A", &"x".repeat(NOTES_LIMIT + 1))
                .await
                .is_err()
        );
        assert!(
            notes
                .write("telegram-1", "A", "key sk-abcdefghijklmnopqrstuvwxyz")
                .await
                .is_err()
        );
        assert!(notes.write("../x", "A", "hi").await.is_err());
        assert!(tables.all_notes().is_empty());
    }
}
