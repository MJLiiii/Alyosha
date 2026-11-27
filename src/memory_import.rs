//! The one-time import of the memory files earlier builds kept in
//! `<state>/memory`: `owner/<name>.md` become `memories` rows and
//! `users/<id>.md` become `user_notes` rows. Migration 2 runs it in its
//! transaction, so it imports everything or nothing, exactly once.
//!
//! A file that cannot be imported fails the import with its reason; nothing
//! is dropped unseen. The rows are read back and compared with the files
//! before the transaction commits. The files are never changed.
//! See `docs/Memory.md#import-the-old-files`.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use chrono::NaiveDate;
use tokio_postgres::Transaction;

use crate::{
    memory::{MemoryRow, NotesRow, valid_name},
    session_store::StoreError,
    user_notes::valid_user_id,
};

/// Where earlier builds kept memory, under the state folder.
pub fn old_folder(state_dir: &Path) -> PathBuf {
    state_dir.join("memory")
}

/// What the old folder holds, parsed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OldFiles {
    /// Each with its path under the folder, such as `owner/editor.md`.
    pub memories: Vec<(String, MemoryRow)>,
    pub notes: Vec<(String, NotesRow)>,
    /// Entries that are not imported, for a warning.
    pub skipped: Vec<String>,
}

/// Reads the folder. `None` when it does not exist; every file that cannot
/// be imported, as `path: reason`, when any cannot.
pub fn read_old_files(folder: &Path) -> Result<Option<OldFiles>, Vec<String>> {
    let listing = match std::fs::read_dir(folder) {
        Ok(listing) => listing,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(vec![format!("the folder cannot be read: {error}")]),
    };
    let mut files = OldFiles::default();
    let mut errors = Vec::new();
    let mut top: Vec<_> = listing.flatten().collect();
    top.sort_by_key(|entry| entry.file_name());
    for entry in top {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name != "owner" && name != "users" {
            files.skipped.push(name);
            continue;
        }
        if !std::fs::symlink_metadata(entry.path()).is_ok_and(|meta| meta.is_dir()) {
            errors.push(format!("{name}: not a folder"));
            continue;
        }
        let listing = match std::fs::read_dir(entry.path()) {
            Ok(listing) => listing,
            Err(error) => {
                errors.push(format!("{name}: cannot be read: {error}"));
                continue;
            }
        };
        let mut inner: Vec<_> = listing.flatten().collect();
        inner.sort_by_key(|entry| entry.file_name());
        for file in inner {
            let file_name = file.file_name().to_string_lossy().into_owned();
            let path = format!("{name}/{file_name}");
            let Some(stem) = file_name.strip_suffix(".md") else {
                files.skipped.push(path);
                continue;
            };
            let parsed = if name == "owner" {
                read_memory(&file.path(), stem).map(|row| files.memories.push((path.clone(), row)))
            } else {
                read_notes(&file.path(), stem).map(|row| files.notes.push((path.clone(), row)))
            };
            if let Err(reason) = parsed {
                errors.push(format!("{path}: {reason}"));
            }
        }
    }
    if errors.is_empty() {
        Ok(Some(files))
    } else {
        Err(errors)
    }
}

/// The file's text, refusing symlinks, other kinds of file, and text that
/// is not UTF-8.
fn read_text(path: &Path) -> Result<String, String> {
    let meta = std::fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if meta.file_type().is_symlink() {
        return Err("a symlink".into());
    }
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let bytes = std::fs::read(path).map_err(|error| error.to_string())?;
    String::from_utf8(bytes).map_err(|_| "not UTF-8".into())
}

/// The header's `key: value` pairs, later ones winning, and the body.
fn split_header(text: &str) -> Result<(HashMap<&str, &str>, &str), String> {
    let rest = text.strip_prefix("---\n").ok_or("no header")?;
    let (header, body) = rest
        .split_once("\n---\n")
        .or_else(|| rest.strip_suffix("\n---").map(|header| (header, "")))
        .ok_or("no end to the header")?;
    let mut fields = HashMap::new();
    for line in header.lines() {
        if let Some((key, value)) = line.split_once(':') {
            fields.insert(key, value.trim());
        }
    }
    Ok((fields, body))
}

fn date(fields: &HashMap<&str, &str>) -> Result<NaiveDate, String> {
    fields
        .get("updated")
        .ok_or("no date")?
        .parse()
        .map_err(|_| "a date that does not parse".into())
}

fn read_memory(path: &Path, name: &str) -> Result<MemoryRow, String> {
    if !valid_name(name) {
        return Err("not a valid memory name".into());
    }
    let text = read_text(path)?;
    let (fields, body) = split_header(&text)?;
    let description = *fields.get("description").ok_or("no description")?;
    if description.is_empty() {
        return Err("an empty description".into());
    }
    Ok(MemoryRow {
        name: name.to_owned(),
        description: description.to_owned(),
        body: body.to_owned(),
        updated: date(&fields)?,
        version: 1,
    })
}

fn read_notes(path: &Path, person: &str) -> Result<NotesRow, String> {
    if !valid_user_id(person) {
        return Err("not a valid user ID".into());
    }
    let text = read_text(path)?;
    let (fields, body) = split_header(&text)?;
    let name = *fields.get("name").ok_or("no name")?;
    Ok(NotesRow {
        person: person.to_owned(),
        name: name.to_owned(),
        notes: body.to_owned(),
        updated: date(&fields)?,
        version: 1,
    })
}

/// Every difference between the files and the rows read back, as
/// `path: what differs`.
pub fn compare(files: &OldFiles, memories: &[MemoryRow], notes: &[NotesRow]) -> Vec<String> {
    let mut differences = Vec::new();
    if memories.len() != files.memories.len() {
        differences.push(format!(
            "memories: {} rows for {} files",
            memories.len(),
            files.memories.len()
        ));
    }
    if notes.len() != files.notes.len() {
        differences.push(format!(
            "user_notes: {} rows for {} files",
            notes.len(),
            files.notes.len()
        ));
    }
    let stored: HashMap<&str, &MemoryRow> = memories
        .iter()
        .map(|row| (row.name.as_str(), row))
        .collect();
    for (path, file) in &files.memories {
        let Some(row) = stored.get(file.name.as_str()) else {
            differences.push(format!("{path}: no row after the import"));
            continue;
        };
        for (field, same) in [
            ("description", row.description == file.description),
            ("date", row.updated == file.updated),
            ("body", row.body == file.body),
            ("version", row.version == 1),
        ] {
            if !same {
                differences.push(format!("{path}: the {field} differs after the import"));
            }
        }
    }
    let stored: HashMap<&str, &NotesRow> =
        notes.iter().map(|row| (row.person.as_str(), row)).collect();
    for (path, file) in &files.notes {
        let Some(row) = stored.get(file.person.as_str()) else {
            differences.push(format!("{path}: no row after the import"));
            continue;
        };
        for (field, same) in [
            ("name", row.name == file.name),
            ("date", row.updated == file.updated),
            ("notes", row.notes == file.notes),
            ("version", row.version == 1),
        ] {
            if !same {
                differences.push(format!("{path}: the {field} differs after the import"));
            }
        }
    }
    differences
}

fn import_error(folder: &Path, problems: &[String]) -> StoreError {
    StoreError::Import(format!(
        "Could not import memory from {}:\n{}",
        folder.display(),
        problems.join("\n")
    ))
}

/// Imports the folder in migration 2's transaction.
pub async fn import(transaction: &Transaction<'_>, folder: &Path) -> Result<(), StoreError> {
    let files = match read_old_files(folder) {
        Ok(Some(files)) => files,
        Ok(None) => return Ok(()),
        Err(errors) => return Err(import_error(folder, &errors)),
    };
    for skipped in &files.skipped {
        tracing::warn!(entry = %skipped, folder = %folder.display(), "not imported into memory");
    }
    for (_, row) in &files.memories {
        transaction
            .execute(
                "insert into memories (name, description, body, updated, version) values ($1, $2, $3, $4, 1)",
                &[&row.name, &row.description, &row.body, &row.updated],
            )
            .await?;
    }
    for (_, row) in &files.notes {
        transaction
            .execute(
                "insert into user_notes (person, name, notes, updated, version) values ($1, $2, $3, $4, 1)",
                &[&row.person, &row.name, &row.notes, &row.updated],
            )
            .await?;
    }
    let memories: Vec<MemoryRow> = transaction
        .query(
            "select name, description, body, updated, version from memories",
            &[],
        )
        .await?
        .iter()
        .map(crate::session_store::memory_row)
        .collect();
    let notes: Vec<NotesRow> = transaction
        .query(
            "select person, name, notes, updated, version from user_notes",
            &[],
        )
        .await?
        .iter()
        .map(crate::session_store::notes_row)
        .collect();
    let differences = compare(&files, &memories, &notes);
    if !differences.is_empty() {
        return Err(import_error(folder, &differences));
    }
    tracing::info!(
        memories = memories.len(),
        notes = notes.len(),
        folder = %folder.display(),
        "Imported {} memories and {} people's notes from {}.",
        memories.len(),
        notes.len(),
        folder.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn sample(root: &Path) {
        write(
            root,
            "owner/thesis-repo.md",
            "---\ndescription: Thesis repository location\nupdated: 2026-11-20\n---\nIn ~/thesis.\nBuild with latexmk.\n\n",
        );
        write(
            root,
            "owner/editor.md",
            "---\ndescription: The owner uses Helix\nupdated: 2026-11-24\n---\nHelix, not Vim.",
        );
        write(root, "owner/notes.txt", "not a memory");
        write(
            root,
            "users/telegram-1.md",
            "---\nname: Bob\nupdated: 2026-11-25\n---\nLikes tea.",
        );
        write(
            root,
            "users/qq-6F0A.md",
            "---\nname: 小明\nupdated: 2026-11-26\n---\nPrefers Chinese.",
        );
    }

    #[test]
    fn a_missing_folder_imports_nothing() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(read_old_files(&root.path().join("memory")), Ok(None));
    }

    #[test]
    fn files_become_rows_with_their_dates() {
        let root = tempfile::tempdir().unwrap();
        sample(root.path());
        let files = read_old_files(root.path()).unwrap().unwrap();
        let names: Vec<&str> = files
            .memories
            .iter()
            .map(|(path, _)| path.as_str())
            .collect();
        assert_eq!(names, ["owner/editor.md", "owner/thesis-repo.md"]);
        let thesis = &files.memories[1].1;
        assert_eq!(thesis.description, "Thesis repository location");
        assert_eq!(thesis.body, "In ~/thesis.\nBuild with latexmk.\n\n");
        assert_eq!(thesis.updated.to_string(), "2026-11-20");
        assert_eq!(thesis.version, 1);
        let people: Vec<(&str, &str)> = files
            .notes
            .iter()
            .map(|(_, row)| (row.person.as_str(), row.name.as_str()))
            .collect();
        assert_eq!(people, [("qq-6F0A", "小明"), ("telegram-1", "Bob")]);
        assert_eq!(files.notes[1].1.notes, "Likes tea.");
        assert_eq!(files.skipped, ["owner/notes.txt"]);
    }

    #[test]
    fn a_bad_file_fails_with_its_reason() {
        for (path, text, reason) in [
            (
                "owner/Bad.md",
                "---\ndescription: x\nupdated: 2026-11-20\n---\n",
                "owner/Bad.md: not a valid memory name",
            ),
            ("owner/loose.md", "just text", "owner/loose.md: no header"),
            (
                "owner/open.md",
                "---\ndescription: x\n",
                "owner/open.md: no end to the header",
            ),
            (
                "owner/empty.md",
                "---\ndescription:\nupdated: 2026-11-20\n---\n",
                "owner/empty.md: an empty description",
            ),
            (
                "owner/nodesc.md",
                "---\nupdated: 2026-11-20\n---\n",
                "owner/nodesc.md: no description",
            ),
            (
                "users/telegram-3.md",
                "---\nname: Eve\n---\nx",
                "users/telegram-3.md: no date",
            ),
            (
                "users/telegram-4.md",
                "---\nname: Eve\nupdated: soon\n---\nx",
                "users/telegram-4.md: a date that does not parse",
            ),
            (
                "users/Eve.md",
                "---\nname: Eve\nupdated: 2026-11-20\n---\nx",
                "users/Eve.md: not a valid user ID",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            sample(root.path());
            write(root.path(), path, text);
            assert_eq!(
                read_old_files(root.path()),
                Err(vec![reason.to_owned()]),
                "{path}"
            );
        }

        let root = tempfile::tempdir().unwrap();
        sample(root.path());
        std::os::unix::fs::symlink(
            root.path().join("owner/editor.md"),
            root.path().join("owner/link.md"),
        )
        .unwrap();
        fs::write(
            root.path().join("owner/latin1.md"),
            b"---\ndescription: caf\xe9\n",
        )
        .unwrap();
        fs::create_dir(root.path().join("owner/dir.md")).unwrap();
        assert_eq!(
            read_old_files(root.path()),
            Err(vec![
                "owner/dir.md: not a regular file".to_owned(),
                "owner/latin1.md: not UTF-8".to_owned(),
                "owner/link.md: a symlink".to_owned(),
            ])
        );
    }

    #[test]
    fn the_comparison_finds_every_difference() {
        let root = tempfile::tempdir().unwrap();
        sample(root.path());
        let files = read_old_files(root.path()).unwrap().unwrap();
        let memories: Vec<MemoryRow> = files.memories.iter().map(|(_, row)| row.clone()).collect();
        let notes: Vec<NotesRow> = files.notes.iter().map(|(_, row)| row.clone()).collect();
        assert!(compare(&files, &memories, &notes).is_empty());

        let mut changed = memories.clone();
        changed[1].body.push('x');
        assert_eq!(
            compare(&files, &changed, &notes),
            ["owner/thesis-repo.md: the body differs after the import"]
        );
        let mut changed = notes.clone();
        changed[0].name = "someone".into();
        changed.pop();
        assert_eq!(
            compare(&files, &memories, &changed),
            [
                "user_notes: 1 rows for 2 files",
                "users/qq-6F0A.md: the name differs after the import",
                "users/telegram-1.md: no row after the import",
            ]
        );
    }
}
