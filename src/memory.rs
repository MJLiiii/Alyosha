//! Memory: curated facts kept across sessions in one global store of Markdown
//! files.
//!
//! The model writes them freely through the `memory` tool and each write shows
//! in the status area. The index goes into the system instruction once per
//! session. See `docs/develop/Memory.md`.

use std::{
    fs::{self, DirBuilder, File},
    io::{Read, Write},
    os::{fd::OwnedFd, unix::fs::DirBuilderExt},
    path::{Path, PathBuf},
    sync::Arc,
};

use chrono::NaiveDate;
use rustix::fs::{AtFlags, Mode, OFlags};
use serde_json::{Value, json};

use crate::tools::{
    RegisteredTool, Scope, ToolContext, ToolDefinition, ToolError, ToolOutput, handler,
};

pub const DESCRIPTION_LIMIT: usize = 160;
pub const BODY_LIMIT: usize = 4 * 1024;
pub const INDEX_BUDGET: usize = 4 * 1024;

/// One readable memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryEntry {
    pub name: String,
    pub description: String,
    pub updated: NaiveDate,
}

/// The index for the system instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryIndex {
    pub text: String,
    pub omitted: usize,
    pub unreadable: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteKind {
    Created,
    Updated,
}

/// Where today's date comes from, in the owner's timezone.
pub type Today = Arc<dyn Fn() -> NaiveDate + Send + Sync>;

/// One scope of the memory store.
pub struct MemoryStore {
    dir: PathBuf,
    today: Today,
}

impl std::fmt::Debug for MemoryStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryStore")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Whether a name is valid: `[a-z0-9][a-z0-9-]{0,63}`.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=64).contains(&bytes.len())
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
}

impl MemoryStore {
    /// The `scope` directory under `root`. A missing directory is an empty
    /// store.
    pub fn open(root: &Path, scope: &str, today: Today) -> Self {
        Self {
            dir: root.join(scope),
            today,
        }
    }

    fn dir_fd(&self) -> Option<OwnedFd> {
        rustix::fs::open(
            &self.dir,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .ok()
    }

    fn read_raw(&self, name: &str) -> Result<Option<String>, ToolError> {
        let Some(dir) = self.dir_fd() else {
            return Ok(None);
        };
        let file = rustix::fs::openat(
            &dir,
            format!("{name}.md"),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        );
        match file {
            Ok(fd) => {
                let mut file = File::from(fd);
                if !file.metadata().is_ok_and(|meta| meta.is_file()) {
                    return Err(ToolError::failed(format!(
                        "Memory {name} is not a regular file."
                    )));
                }
                let mut text = String::new();
                file.read_to_string(&mut text).map_err(|error| {
                    ToolError::failed(format!("Could not read memory {name}: {error}"))
                })?;
                Ok(Some(text))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(rustix::io::Errno::LOOP) => {
                Err(ToolError::failed(format!("Memory {name} is a symlink.")))
            }
            Err(errno) => Err(ToolError::failed(format!(
                "Could not read memory {name}: {errno}"
            ))),
        }
    }

    /// Every readable entry, and the names of files without a valid header.
    fn scan(&self) -> (Vec<MemoryEntry>, Vec<String>) {
        let mut entries = Vec::new();
        let mut unreadable = Vec::new();
        let Ok(listing) = fs::read_dir(&self.dir) else {
            return (entries, unreadable);
        };
        for item in listing.flatten() {
            let file_name = item.file_name().to_string_lossy().into_owned();
            let Some(name) = file_name.strip_suffix(".md") else {
                continue;
            };
            if !valid_name(name) {
                continue;
            }
            match self
                .read_raw(name)
                .ok()
                .flatten()
                .as_deref()
                .and_then(parse_memory)
            {
                Some((description, updated, _)) => entries.push(MemoryEntry {
                    name: name.to_owned(),
                    description,
                    updated,
                }),
                None => unreadable.push(name.to_owned()),
            }
        }
        entries.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.name.cmp(&b.name)));
        unreadable.sort();
        (entries, unreadable)
    }

    pub fn list(&self) -> Vec<MemoryEntry> {
        self.scan().0
    }

    pub fn index(&self) -> MemoryIndex {
        let (entries, unreadable) = self.scan();
        let (text, omitted) = index_text(&entries);
        MemoryIndex {
            text,
            omitted,
            unreadable,
        }
    }

    /// The file text, header included, even when the header is invalid.
    pub fn read(&self, name: &str) -> Result<String, ToolError> {
        check_name(name)?;
        self.read_raw(name)?
            .ok_or_else(|| ToolError::failed(format!("There is no memory named {name}.")))
    }

    pub fn write(
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
        let (entries, _) = self.scan();
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
        let kind = if self.read_raw(name)?.is_some() {
            WriteKind::Updated
        } else {
            WriteKind::Created
        };
        let text = format!("---\ndescription: {description}\nupdated: {today}\n---\n{content}");
        self.store(name, text.as_bytes())?;
        Ok(kind)
    }

    pub fn delete(&self, name: &str) -> Result<(), ToolError> {
        check_name(name)?;
        let dir = self
            .dir_fd()
            .ok_or_else(|| ToolError::failed(format!("There is no memory named {name}.")))?;
        rustix::fs::unlinkat(&dir, format!("{name}.md"), AtFlags::empty()).map_err(|errno| {
            match errno {
                rustix::io::Errno::NOENT => {
                    ToolError::failed(format!("There is no memory named {name}."))
                }
                other => ToolError::failed(format!("Could not delete memory {name}: {other}")),
            }
        })
    }

    /// Stages the file in the scope directory and renames it into place.
    fn store(&self, name: &str, bytes: &[u8]) -> Result<(), ToolError> {
        let failed =
            |error: String| ToolError::failed(format!("Could not save memory {name}: {error}"));
        DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)
            .map_err(|error| failed(error.to_string()))?;
        let dir = self
            .dir_fd()
            .ok_or_else(|| failed("the folder cannot be opened".into()))?;
        let staging = format!(".{name}.{}.tmp", crate::environment::random_suffix());
        let fd = rustix::fs::openat(
            &dir,
            &staging,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )
        .map_err(|errno| failed(errno.to_string()))?;
        let mut file = File::from(fd);
        let result = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| error.to_string())
            .and_then(|()| {
                rustix::fs::renameat(&dir, &staging, &dir, format!("{name}.md"))
                    .map_err(|errno| errno.to_string())
            });
        if let Err(error) = result {
            let _ = rustix::fs::unlinkat(&dir, &staging, AtFlags::empty());
            return Err(failed(error));
        }
        Ok(())
    }
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

/// The description, date, and body of a memory file.
fn parse_memory(text: &str) -> Option<(String, NaiveDate, String)> {
    let rest = text.strip_prefix("---\n")?;
    let (header, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|h| (h, "")))?;
    let mut description = None;
    let mut updated = None;
    for line in header.lines() {
        if let Some(value) = line.strip_prefix("description:") {
            description = Some(value.trim().to_owned());
        } else if let Some(value) = line.strip_prefix("updated:") {
            updated = value.trim().parse::<NaiveDate>().ok();
        }
    }
    Some((
        description.filter(|d| !d.is_empty())?,
        updated?,
        body.to_owned(),
    ))
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
fn credential_pattern(text: &str) -> Option<&'static str> {
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
            description: "Your long-term memory, shared across sessions. `list` shows every memory, `read` returns one, `write` creates or replaces a whole memory from name, description (one line), and content, and `delete` removes one. Names use lowercase letters, digits, and hyphens.".into(),
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
                    let entries = store.list();
                    if entries.is_empty() {
                        return Ok(ToolOutput::text("No memories yet."));
                    }
                    Ok(ToolOutput::text(entries.iter().map(line).collect::<String>()))
                }
                "read" => Ok(ToolOutput::text(store.read(need_name()?)?)),
                "write" => {
                    let name = need_name()?;
                    let description = arguments["description"]
                        .as_str()
                        .ok_or_else(|| ToolError::failed("write needs a description."))?;
                    let content = arguments["content"]
                        .as_str()
                        .ok_or_else(|| ToolError::failed("write needs content."))?;
                    let kind = store.write(name, description, content)?;
                    let text = if kind == WriteKind::Created { "created" } else { "updated" };
                    Ok(ToolOutput::text(text).with_notice(format!("Remembered: {name}")))
                }
                "delete" => {
                    let name = need_name()?;
                    store.delete(name)?;
                    Ok(ToolOutput::text(format!("Deleted {name}.")).with_notice(format!("Forgot: {name}")))
                }
                other => Err(ToolError::failed(format!("Unknown memory command {other:?}."))),
            }
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn store(root: &Path, date: &str) -> MemoryStore {
        let date: NaiveDate = date.parse().unwrap();
        MemoryStore::open(root, "owner", Arc::new(move || date))
    }

    #[test]
    fn an_empty_store_then_a_first_write() {
        let root = tempfile::tempdir().unwrap();
        let memory_root = root.path().join("memory");
        let store = store(&memory_root, "2026-11-25");
        assert_eq!(
            instruction_section(&store.index()).lines().last(),
            Some("No memories yet.")
        );
        assert_eq!(
            store
                .write(
                    "thesis-repo",
                    "Thesis lives in ~/Projects/thesis",
                    "Use LaTeX."
                )
                .unwrap(),
            WriteKind::Created
        );
        let dir = memory_root.join("owner");
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join("thesis-repo.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            store.read("thesis-repo").unwrap(),
            "---\ndescription: Thesis lives in ~/Projects/thesis\nupdated: 2026-11-25\n---\nUse LaTeX."
        );
    }

    #[test]
    fn an_update_takes_the_new_date() {
        let root = tempfile::tempdir().unwrap();
        store(root.path(), "2026-11-20")
            .write("editor", "Uses Helix", "x")
            .unwrap();
        let later = store(root.path(), "2026-11-25");
        assert_eq!(
            later.write("editor", "Uses Helix and Zed", "y").unwrap(),
            WriteKind::Updated
        );
        let entries = later.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].updated.to_string(), "2026-11-25");
        assert_eq!(
            later.index().text,
            "- editor (2026-11-25): Uses Helix and Zed\n"
        );
    }

    #[test]
    fn bad_names_and_symlinks_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path(), "2026-11-25");
        for name in ["../x", "A", &"a".repeat(65), ""] {
            assert!(store.write(name, "d", "c").is_err(), "{name}");
        }
        fs::create_dir_all(root.path().join("owner")).unwrap();
        fs::write(root.path().join("outside.md"), "secret").unwrap();
        std::os::unix::fs::symlink(
            root.path().join("outside.md"),
            root.path().join("owner/x.md"),
        )
        .unwrap();
        assert!(store.read("x").is_err());
        assert_eq!(
            fs::read_to_string(root.path().join("outside.md")).unwrap(),
            "secret"
        );
    }

    #[test]
    fn oversized_and_secret_text_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path(), "2026-11-25");
        store.write("keep", "original", "body").unwrap();
        let before = store.read("keep").unwrap();
        assert!(store.write("keep", &"d".repeat(161), "body").is_err());
        assert!(store.write("keep", "two\nlines", "body").is_err());
        assert!(store.write("keep", "fine", &"b".repeat(4097)).is_err());
        assert!(
            store
                .write("keep", "fine", "key sk-abcdefghijklmnopqrstuvwxyz")
                .is_err()
        );
        assert!(
            store
                .write("keep", "fine", "-----BEGIN OPENSSH PRIVATE KEY-----")
                .is_err()
        );
        assert!(store.write("keep", "fine", "AKIAABCDEFGHIJKLMNOP").is_err());
        assert_eq!(store.read("keep").unwrap(), before);
    }

    #[test]
    fn a_full_index_refuses_growth_but_allows_same_size_updates() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path(), "2026-11-25");
        let description = "d".repeat(150);
        let mut written = 0;
        while store
            .write(&format!("m{written:03}"), &description, "x")
            .is_ok()
        {
            written += 1;
        }
        assert!(written > 10);
        let error = store.write("one-more", &description, "x").unwrap_err();
        assert!(error.to_string().contains("Merge"), "{error}");
        store.write("m000", &description, "changed").unwrap();

        // Hand-edited files beyond the budget: newest first, then the count.
        for extra in 0..10 {
            fs::write(
                root.path().join(format!("owner/old{extra}.md")),
                format!("---\ndescription: {description}\nupdated: 2020-01-01\n---\nx"),
            )
            .unwrap();
        }
        let index = store.index();
        assert!(index.omitted >= 10);
        assert!(index.text.starts_with("- m000 (2026-11-25)"));
        assert!(index.text.ends_with(&format!(
            "{} more memories are not shown. Use memory list.\n",
            index.omitted
        )));
        assert!(index.text.len() <= INDEX_BUDGET + 100);
    }

    #[test]
    fn a_file_without_a_header_is_skipped_but_readable() {
        let root = tempfile::tempdir().unwrap();
        let store = store(root.path(), "2026-11-25");
        fs::create_dir_all(root.path().join("owner")).unwrap();
        fs::write(root.path().join("owner/loose.md"), "just text").unwrap();
        let index = store.index();
        assert_eq!(index.unreadable, ["loose"]);
        assert!(index.text.is_empty());
        assert_eq!(store.read("loose").unwrap(), "just text");
    }
}
