//! Notes on the people who talk to Alyosha in group chats: one Markdown file
//! per person, keyed by platform and user ID, in `memory/users/`.
//!
//! Only guest sessions open this store. Each message reaches the model with
//! its sender's notes in front of it, and the `user_memory` tool rewrites the
//! notes of the person whose message the turn answers, never anyone else's.
//! See `docs/Memory.md#peoples-notes`.

use std::path::Path;

use chrono::NaiveDate;
use serde_json::{Value, json};

use crate::{
    agent_state::Sender,
    memory::{MemoryStore, Today, WriteKind, credential_pattern},
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

/// The `users` directory of the memory store.
pub struct UserNotes {
    store: MemoryStore,
    today: Today,
}

impl std::fmt::Debug for UserNotes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UserNotes")
            .field("store", &self.store)
            .finish_non_exhaustive()
    }
}

impl UserNotes {
    /// `<root>/users`. A missing directory holds no notes.
    pub fn open(root: &Path, today: Today) -> Self {
        Self {
            store: MemoryStore::open(root, "users", today.clone()),
            today,
        }
    }

    fn check(id: &str) -> Result<(), ToolError> {
        if valid_user_id(id) {
            Ok(())
        } else {
            Err(ToolError::failed(format!("{id:?} is not a valid user ID.")))
        }
    }

    /// The person's notes, or `None` if there are none or the file has no
    /// valid header.
    pub fn read(&self, id: &str) -> Result<Option<Notes>, ToolError> {
        Self::check(id)?;
        Ok(self.store.read_raw(id)?.as_deref().and_then(parse_notes))
    }

    /// Replaces the person's notes and records their current name.
    pub fn write(&self, id: &str, name: &str, notes: &str) -> Result<WriteKind, ToolError> {
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
        let kind = if self.store.read_raw(id)?.is_some() {
            WriteKind::Updated
        } else {
            WriteKind::Created
        };
        let text = format!(
            "---\nname: {}\nupdated: {}\n---\n{}",
            clean_name(name),
            (self.today)(),
            notes.trim_end()
        );
        self.store.store(id, text.as_bytes())?;
        Ok(kind)
    }

    /// What goes in front of a message from `sender`: their notes, then the
    /// message as `<name>: <text>`.
    pub fn opening(&self, sender: &Sender, text: &str) -> String {
        let notes = match self.read(&sender.id) {
            Ok(Some(notes)) if !notes.text.trim().is_empty() => format!(
                "Notes on this person ({}, updated {}):\n{}",
                sender.id,
                notes.updated,
                notes.text.trim_end()
            ),
            Ok(_) => format!("Notes on this person ({}): No notes yet.", sender.id),
            Err(error) => {
                tracing::warn!(user = %sender.id, error = %error, "notes could not be read");
                format!("Notes on this person ({}): unreadable.", sender.id)
            }
        };
        format!("{notes}\n\n{}: {text}", clean_name(&sender.name))
    }
}

fn parse_notes(text: &str) -> Option<Notes> {
    let rest = text.strip_prefix("---\n")?;
    let (header, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|h| (h, "")))?;
    let mut name = None;
    let mut updated = None;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("name:") {
            name = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("updated:") {
            updated = value.trim().parse::<NaiveDate>().ok();
        }
    }
    Some(Notes {
        name: name?,
        updated: updated?,
        text: body.to_owned(),
    })
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
            let notes = arguments["notes"].as_str().unwrap_or_default();
            store.write(&speaker.id, &speaker.name, notes)?;
            Ok(ToolOutput::text("saved")
                .with_notice(format!("Remembered: {}", clean_name(&speaker.name))))
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn store(root: &Path) -> UserNotes {
        let day = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
        UserNotes::open(root, Arc::new(move || day))
    }

    fn bob() -> Sender {
        Sender {
            id: "telegram-123".into(),
            name: "Bob".into(),
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

    #[test]
    fn notes_are_written_read_and_shown_before_the_message() {
        let root = tempfile::tempdir().unwrap();
        let notes = store(root.path());
        assert_eq!(notes.read("telegram-123").unwrap(), None);
        assert_eq!(
            notes.opening(&bob(), "hi"),
            "Notes on this person (telegram-123): No notes yet.\n\nBob: hi"
        );
        assert_eq!(
            notes
                .write("telegram-123", "Bob\nupdated: 1999-01-01", "Likes tea.")
                .unwrap(),
            WriteKind::Created
        );
        let read = notes.read("telegram-123").unwrap().unwrap();
        assert_eq!(read.name, "Bob updated: 1999-01-01", "one line");
        assert_eq!(read.updated, NaiveDate::from_ymd_opt(2026, 11, 25).unwrap());
        assert_eq!(read.text, "Likes tea.");
        assert_eq!(
            notes.opening(&bob(), "hi"),
            "Notes on this person (telegram-123, updated 2026-11-25):\nLikes tea.\n\nBob: hi"
        );
        assert_eq!(
            notes.write("telegram-123", "Bob", "x").unwrap(),
            WriteKind::Updated
        );
        let file = root.path().join("users/telegram-123.md");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn oversized_secret_and_invalid_writes_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let notes = store(root.path());
        assert!(
            notes
                .write("telegram-1", "A", &"x".repeat(NOTES_LIMIT + 1))
                .is_err()
        );
        assert!(
            notes
                .write("telegram-1", "A", "key sk-abcdefghijklmnopqrstuvwxyz")
                .is_err()
        );
        assert!(notes.write("../x", "A", "hi").is_err());
        assert!(!root.path().join("users").exists());
    }
}
