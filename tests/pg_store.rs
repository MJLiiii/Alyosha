//! `PgStore` against a throwaway PostgreSQL cluster, started with `initdb`
//! and `pg_ctl` in a temporary folder. Without them on `PATH`, or without
//! pgvector, which recall's migration needs, the test prints a skip notice
//! and passes. The dev shell's PostgreSQL has pgvector. See
//! `docs/SessionStore.md#verification`.

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::{Duration, SystemTime},
};

use alyosha::{
    agent_state::{
        AssistantEntry, AssistantPart, HistoryEntry, MessageId, MessageOrigin, Replay, ToolCall,
        ToolOutcome, ToolResultEntry, UserEntry,
    },
    memory::{MemoryStore, MemoryTables},
    recall::{EntryRef, RecallScope},
    session_store::{PendingRow, PgStore, SessionStore, StoreError, encode},
    user_notes::UserNotes,
};
use chrono::NaiveDate;
use serde_json::json;

/// A memory folder that does not exist: nothing to import.
const NO_FOLDER: &str = "/nonexistent/alyosha-memory";

/// A running cluster, stopped when dropped.
struct Cluster {
    _root: tempfile::TempDir,
    data: PathBuf,
    socket: PathBuf,
}

impl Cluster {
    fn start() -> Option<Self> {
        if Command::new("initdb").arg("--version").output().is_err() {
            eprintln!("skipped: initdb is not on PATH");
            return None;
        }
        // Unix socket paths are short; keep the folder near the root.
        let root = tempfile::Builder::new()
            .prefix("alyosha-pg")
            .tempdir_in("/tmp")
            .unwrap();
        let data = root.path().join("data");
        let socket = root.path().join("s");
        std::fs::create_dir(&socket).unwrap();
        let user = std::env::var("USER").unwrap();
        let status = Command::new("initdb")
            .args([
                "-A",
                "trust",
                "-E",
                "UTF8",
                "--no-locale",
                "-U",
                &user,
                "-D",
            ])
            .arg(&data)
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        let status = Command::new("pg_ctl")
            .arg("-D")
            .arg(&data)
            .arg("-o")
            .arg(format!("-k {} -c listen_addresses=''", socket.display()))
            .args(["-w", "-l"])
            .arg(root.path().join("log"))
            .arg("start")
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
        let cluster = Self {
            _root: root,
            data,
            socket,
        };
        let vector = Command::new("psql")
            .arg("-h")
            .arg(&cluster.socket)
            .args([
                "-At",
                "-d",
                "postgres",
                "-c",
                "create extension if not exists vector",
            ])
            .output()
            .unwrap();
        if !vector.status.success() {
            eprintln!("skipped: this PostgreSQL has no pgvector; use the dev shell's");
            return None;
        }
        Some(cluster)
    }

    fn target(&self, database: &str) -> String {
        format!("host={} dbname={database}", self.socket.display())
    }

    fn create_database(&self, name: &str) {
        let status = Command::new("createdb")
            .arg("-h")
            .arg(&self.socket)
            .arg(name)
            .output()
            .unwrap();
        assert!(status.status.success(), "{status:?}");
    }

    fn psql(&self, database: &str, sql: &str) -> String {
        let output = Command::new("psql")
            .arg("-h")
            .arg(&self.socket)
            .args(["-At", "-d", database, "-c", sql])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = Command::new("pg_ctl")
            .arg("-D")
            .arg(&self.data)
            .args(["-m", "immediate", "stop"])
            .output();
    }
}

fn sample() -> Vec<HistoryEntry> {
    vec![
        HistoryEntry::User(UserEntry::text_only(
            MessageId(3),
            MessageOrigin::Owner,
            "[clock]\nread a",
        )),
        HistoryEntry::Assistant(AssistantEntry {
            parts: vec![
                // Key order and spacing that jsonb would change.
                AssistantPart::Replay(Replay {
                    route: "responses".into(),
                    data: json!({"z": 1, "a": {"y": [1, 2], "b": "x"}, "type": "reasoning"}),
                }),
                AssistantPart::ToolCall(ToolCall {
                    id: "call_a".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.txt"}),
                }),
            ],
        }),
        HistoryEntry::ToolResult(ToolResultEntry {
            call_id: "call_a".into(),
            outcome: ToolOutcome::Success("alpha".into()),
            images: Vec::new(),
        }),
        HistoryEntry::Assistant(AssistantEntry {
            parts: vec![AssistantPart::Text("It says alpha.".into())],
        }),
    ]
}

#[tokio::test]
async fn pg_store_saves_loads_and_locks() {
    let Some(cluster) = Cluster::start() else {
        return;
    };

    // Two processes starting together migrate once.
    cluster.create_database("migrate");
    let target = cluster.target("migrate");
    let (first, second) = tokio::join!(
        PgStore::connect(&target, Path::new(NO_FOLDER)),
        PgStore::connect(&target, Path::new(NO_FOLDER))
    );
    first.unwrap();
    second.unwrap();
    assert_eq!(
        cluster.psql(
            "migrate",
            "select string_agg(version::text, ',' order by version) from schema_version"
        ),
        "1,2,3,4,5"
    );
    cluster.psql("migrate", "insert into schema_version values (99)");
    let Err(StoreError::Failed(error)) =
        PgStore::connect(&cluster.target("migrate"), Path::new(NO_FOLDER)).await
    else {
        panic!("a newer database is refused");
    };
    assert!(error.contains("newer"), "{error}");

    // Append, checkpoint, and resume.
    cluster.create_database("alyosha");
    let store = PgStore::connect(&cluster.target("alyosha"), Path::new(NO_FOLDER))
        .await
        .unwrap();
    let created = store
        .create("owner:telegram", Path::new("/w"))
        .await
        .unwrap();
    assert!(store.load_current("nobody").await.unwrap().is_none());
    for (seq, entry) in sample().iter().enumerate() {
        store.append(created.id, seq as u64, entry).await.unwrap();
    }
    let duplicate = store.append(created.id, 0, &sample()[0]).await;
    assert!(duplicate.is_err(), "a sequence number is stored once");
    store
        .save_checkpoint(created.id, 2, "SUMMARY one")
        .await
        .unwrap();
    store
        .save_checkpoint(created.id, 2, "SUMMARY two")
        .await
        .unwrap();
    store.close(created.id).await.unwrap();

    let loaded = store.load_current("owner:telegram").await.unwrap().unwrap();
    assert_eq!(loaded.id, created.id);
    assert_eq!(loaded.checkpoint, Some((2, "SUMMARY two".to_owned())));
    assert_eq!(loaded.entries, sample()[2..]);
    assert_eq!(loaded.next_seq, 4);
    assert_eq!(loaded.next_message_id, 4);
    // The replay data comes back exactly as it was written.
    let stored = cluster.psql(
        "alyosha",
        "select entry->'parts'->0->'value'->>'data' from history where seq = 1",
    );
    assert_eq!(
        stored,
        r#"{"z":1,"a":{"y":[1,2],"b":"x"},"type":"reasoning"}"#
    );

    // Another process cannot open a held session.
    let other = PgStore::connect(&cluster.target("alyosha"), Path::new(NO_FOLDER))
        .await
        .unwrap();
    assert_eq!(
        other.load_current("owner:telegram").await.unwrap_err(),
        StoreError::Busy
    );
    // The lock goes with the connection, as when a process dies.
    drop(store);
    let mut reopened = None;
    for _ in 0..50 {
        match other.load_current("owner:telegram").await {
            Ok(session) => {
                reopened = session;
                break;
            }
            Err(StoreError::Busy) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(error) => panic!("{error}"),
        }
    }
    assert!(reopened.is_some(), "the lock was released");

    // A new session under the key becomes its current one.
    let newer = other
        .create("owner:telegram", Path::new("/w"))
        .await
        .unwrap();
    other.close(newer.id).await.unwrap();
    other.close(created.id).await.unwrap();
    let current = other.load_current("owner:telegram").await.unwrap().unwrap();
    assert_eq!(current.id, newer.id);
    assert!(current.entries.is_empty());
}

fn put(root: &Path, path: &str, text: &str) {
    let path = root.join(path);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

/// A state folder whose `memory` holds what an earlier build wrote.
fn old_files() -> tempfile::TempDir {
    let state = tempfile::tempdir().unwrap();
    let memory = state.path().join("memory");
    put(
        &memory,
        "owner/thesis-repo.md",
        "---\ndescription: Thesis repository location\nupdated: 2026-11-20\n---\nIn ~/thesis.\nBuild with latexmk.\n\n",
    );
    put(
        &memory,
        "owner/editor.md",
        "---\ndescription: The owner uses Helix\nupdated: 2026-11-24\n---\nHelix, not Vim.",
    );
    put(&memory, "owner/notes.txt", "not a memory");
    put(
        &memory,
        "users/telegram-1.md",
        "---\nname: Bob\nupdated: 2026-11-25\n---\nLikes tea.",
    );
    put(
        &memory,
        "users/qq-6F0A.md",
        "---\nname: 小明\nupdated: 2026-11-26\n---\nPrefers Chinese.",
    );
    state
}

/// Each file's bytes and modification time.
fn snapshot(folder: &Path) -> Vec<(PathBuf, Vec<u8>, SystemTime)> {
    let mut files = Vec::new();
    for sub in ["owner", "users"] {
        for entry in fs::read_dir(folder.join(sub)).unwrap().flatten() {
            let meta = entry.metadata().unwrap();
            files.push((
                entry.path(),
                fs::read(entry.path()).unwrap(),
                meta.modified().unwrap(),
            ));
        }
    }
    files.sort();
    files
}

#[tokio::test]
async fn memory_files_are_imported_once_and_checked() {
    let Some(cluster) = Cluster::start() else {
        return;
    };

    // Without a folder: empty tables at the newest version.
    cluster.create_database("empty");
    PgStore::connect(&cluster.target("empty"), Path::new(NO_FOLDER))
        .await
        .unwrap();
    assert_eq!(
        cluster.psql("empty", "select max(version) from schema_version"),
        "5"
    );
    assert_eq!(
        cluster.psql(
            "empty",
            "select (select count(*) from memories) + (select count(*) from user_notes)"
        ),
        "0"
    );

    // Two processes start together: one import, equal to the files.
    let state = old_files();
    let folder = state.path().join("memory");
    let before = snapshot(&folder);
    cluster.create_database("imported");
    let target = cluster.target("imported");
    let (first, second) = tokio::join!(
        PgStore::connect(&target, &folder),
        PgStore::connect(&target, &folder)
    );
    first.unwrap();
    let store = second.unwrap();
    assert_eq!(
        cluster.psql(
            "imported",
            "select string_agg(name || '|' || description || '|' || updated || '|' || version, ';' order by name) from memories"
        ),
        "editor|The owner uses Helix|2026-11-24|1;thesis-repo|Thesis repository location|2026-11-20|1"
    );
    assert_eq!(
        cluster.psql(
            "imported",
            "select string_agg(person || '|' || name || '|' || notes || '|' || updated || '|' || version, ';' order by person) from user_notes"
        ),
        "qq-6F0A|小明|Prefers Chinese.|2026-11-26|1;telegram-1|Bob|Likes tea.|2026-11-25|1"
    );
    assert_eq!(
        store.memory("thesis-repo").await.unwrap().unwrap().body,
        "In ~/thesis.\nBuild with latexmk.\n\n"
    );
    assert_eq!(snapshot(&folder), before, "the files are untouched");

    // A later start imports nothing, even after a file changed.
    put(
        &folder,
        "owner/editor.md",
        "---\ndescription: Changed\nupdated: 2026-11-27\n---\nZed.",
    );
    let again = PgStore::connect(&target, &folder).await.unwrap();
    assert_eq!(
        again.memory("editor").await.unwrap().unwrap().body,
        "Helix, not Vim."
    );

    // The host's list.
    let output = Command::new(env!("CARGO_BIN_EXE_alyosha"))
        .arg("memory-list")
        .env("ALYOSHA_DATABASE", &target)
        .env("ALYOSHA_STATE", state.path())
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "editor (2026-11-24): The owner uses Helix\nthesis-repo (2026-11-20): Thesis repository location\n2 memories · 2 people with notes\n"
    );

    // A dump restored into an emptied database imports nothing again.
    cluster.create_database("restored");
    let restore = Command::new("sh")
        .arg("-c")
        .arg(format!(
            "pg_dump -h '{socket}' imported | psql -q -h '{socket}' -d restored",
            socket = cluster.socket.display()
        ))
        .output()
        .unwrap();
    assert!(restore.status.success(), "{restore:?}");
    let dump = Command::new("pg_dump")
        .arg("-h")
        .arg(&cluster.socket)
        .arg("imported")
        .output()
        .unwrap();
    let dump = String::from_utf8(dump.stdout).unwrap();
    assert!(
        dump.contains("thesis-repo") && dump.contains("telegram-1"),
        "the dump holds the rows"
    );
    let restored = PgStore::connect(&cluster.target("restored"), &folder)
        .await
        .unwrap();
    assert_eq!(restored.memories().await.unwrap().len(), 2);
    assert_eq!(
        restored.memory("editor").await.unwrap().unwrap().body,
        "Helix, not Vim."
    );

    // A bad file fails the whole import, naming the file and why.
    for (index, (path, text, reason)) in [
        (
            "owner/Bad.md",
            Some("---\ndescription: x\nupdated: 2026-11-20\n---\n"),
            "owner/Bad.md: not a valid memory name",
        ),
        ("owner/link.md", None, "owner/link.md: a symlink"),
        (
            "users/telegram-3.md",
            Some("---\nname: Eve\n---\nx"),
            "users/telegram-3.md: no date",
        ),
        (
            "owner/empty.md",
            Some("---\ndescription:\nupdated: 2026-11-20\n---\n"),
            "owner/empty.md: an empty description",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let state = old_files();
        let folder = state.path().join("memory");
        match text {
            Some(text) => put(&folder, path, text),
            None => std::os::unix::fs::symlink(folder.join("owner/editor.md"), folder.join(path))
                .unwrap(),
        }
        let database = format!("bad{index}");
        cluster.create_database(&database);
        let target = cluster.target(&database);
        let Err(StoreError::Import(message)) = PgStore::connect(&target, &folder).await else {
            panic!("{path} fails the import");
        };
        assert!(
            message.starts_with("Could not import memory from "),
            "{message}"
        );
        assert!(message.ends_with(&format!(":\n{reason}")), "{message}");
        assert_eq!(
            cluster.psql(&database, "select max(version) from schema_version"),
            "1"
        );
        assert_eq!(
            cluster.psql(&database, "select to_regclass('memories') is null"),
            "t"
        );
        fs::remove_file(folder.join(path)).unwrap();
        let store = PgStore::connect(&target, &folder).await.unwrap();
        assert_eq!(store.memories().await.unwrap().len(), 2, "{path}");
        assert_eq!(store.count_notes().await.unwrap(), 2, "{path}");
    }
}

#[tokio::test]
async fn memory_versions_hold_across_connections() {
    let Some(cluster) = Cluster::start() else {
        return;
    };
    cluster.create_database("alyosha");
    let target = cluster.target("alyosha");
    let a_tables: Arc<dyn MemoryTables> = Arc::new(
        PgStore::connect(&target, Path::new(NO_FOLDER))
            .await
            .unwrap(),
    );
    let b_tables: Arc<dyn MemoryTables> = Arc::new(
        PgStore::connect(&target, Path::new(NO_FOLDER))
            .await
            .unwrap(),
    );
    let day = NaiveDate::from_ymd_opt(2026, 11, 25).unwrap();
    let today = Arc::new(move || day);
    let a = MemoryStore::open(a_tables.clone(), today.clone());
    let b = MemoryStore::open(b_tables.clone(), today.clone());

    a.write("thesis-repo", "Where", "A").await.unwrap();
    a.read("thesis-repo").await.unwrap();
    b.read("thesis-repo").await.unwrap();
    b.write("thesis-repo", "Where", "B").await.unwrap();
    let error = a.write("thesis-repo", "Where", "A2").await.unwrap_err();
    assert!(
        error.to_string().contains("changed since you read it"),
        "{error}"
    );
    assert!(a.delete("thesis-repo").await.is_err());
    assert!(a.read("thesis-repo").await.unwrap().ends_with("\nB"));
    a.write("thesis-repo", "Where", "A2").await.unwrap();
    assert_eq!(
        cluster.psql("alyosha", "select body || version from memories"),
        "A23"
    );
    // A new session, like a resumed one, reads before it writes.
    let resumed = MemoryStore::open(b_tables.clone(), today.clone());
    assert!(resumed.write("thesis-repo", "Where", "C").await.is_err());
    resumed.read("thesis-repo").await.unwrap();
    resumed.delete("thesis-repo").await.unwrap();
    assert_eq!(
        cluster.psql("alyosha", "select count(*) from memories"),
        "0"
    );

    // People's notes: a change in another chat returns the newer notes.
    let bob = alyosha::agent_state::Sender {
        id: "telegram-2".into(),
        name: "Bob".into(),
        owner: false,
    };
    let here = UserNotes::open(a_tables, today.clone());
    let there = UserNotes::open(b_tables, today);
    here.opening(&bob, "hi").await;
    there.opening(&bob, "hello").await;
    there
        .write("telegram-2", "Bob", "Studies physics.")
        .await
        .unwrap();
    let error = here
        .write("telegram-2", "Bob", "Likes tea.")
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Studies physics."), "{error}");
    here.write("telegram-2", "Bob", "Studies physics. Likes tea.")
        .await
        .unwrap();
    assert_eq!(
        cluster.psql("alyosha", "select notes || '|' || version from user_notes"),
        "Studies physics. Likes tea.|2"
    );
}

#[tokio::test]
async fn media_rows_are_kept_once_by_hash() {
    use alyosha::media::MediaStore;
    let Some(cluster) = Cluster::start() else {
        return;
    };
    cluster.create_database("media");
    let store = PgStore::connect(&cluster.target("media"), Path::new(NO_FOLDER))
        .await
        .unwrap();
    let bytes = vec![0u8, 1, 2, 255];
    let first = store.put_media("image/png", &bytes).await.unwrap();
    let second = store.put_media("image/png", &bytes).await.unwrap();
    assert_eq!(first, second);
    assert_eq!(first, alyosha::media::sha256_hex(&bytes));
    assert_eq!(
        store.get_media(&first).await.unwrap(),
        Some(("image/png".to_owned(), bytes))
    );
    assert_eq!(store.get_media("missing").await.unwrap(), None);
    assert_eq!(cluster.psql("media", "select count(*) from media"), "1");
}

/// A vector of pgvector's 1,536 dimensions pointing along `axis`.
fn axis(axis: usize) -> Vec<f32> {
    let mut vector = vec![0.0; 1536];
    vector[axis] = 1.0;
    vector
}

#[tokio::test]
async fn recall_migration_fills_text_and_searches_with_pgvector() {
    let Some(cluster) = Cluster::start() else {
        return;
    };
    // A database at version 3 with one saved entry.
    cluster.create_database("recall");
    for sql in [
        include_str!("../migrations/001_sessions.sql"),
        include_str!("../migrations/002_memory.sql"),
        include_str!("../migrations/003_media.sql"),
    ] {
        cluster.psql("recall", sql);
    }
    let old = encode(&HistoryEntry::User(UserEntry::text_only(
        MessageId(1),
        MessageOrigin::Owner,
        "[Wed 2026-11-25 10:02, Europe/Amsterdam]\nMy Router dropped the 路由 link",
    )));
    cluster.psql(
        "recall",
        &format!(
            "create table schema_version (version integer not null);
             insert into schema_version values (1), (2), (3);
             insert into sessions (key, cwd) values ('owner:telegram', '/');
             insert into history (session_id, seq, kind, message_id, entry)
             values (1, 0, 'user', 1, '{}');",
            old.to_string().replace('\'', "''")
        ),
    );

    let store = PgStore::connect(&cluster.target("recall"), Path::new(NO_FOLDER))
        .await
        .unwrap();
    assert_eq!(
        cluster.psql("recall", "select text from history where seq = 0"),
        "My Router dropped the 路由 link"
    );
    let group = store.create("group:qq:G", Path::new("/")).await.unwrap();
    store
        .append(
            group.id,
            0,
            &HistoryEntry::Assistant(AssistantEntry {
                parts: vec![AssistantPart::Text("100% picnic_day".into())],
            }),
        )
        .await
        .unwrap();
    let current = store.create("owner:qq", Path::new("/")).await.unwrap();

    let words = |scope: &'static RecallScope, terms: &'static [&'static str]| {
        let store = &store;
        async move {
            let terms: Vec<String> = terms.iter().map(|t| (*t).to_owned()).collect();
            store
                .recall_words(scope, current.id, &terms, 20)
                .await
                .unwrap()
                .into_iter()
                .map(|hit| (hit.key, hit.entry.seq))
                .collect::<Vec<_>>()
        }
    };
    static OWNER: RecallScope = RecallScope::Owner;
    assert_eq!(
        words(&OWNER, &["router", "路由"]).await,
        [("owner:telegram".to_owned(), 0)]
    );
    assert_eq!(
        words(&OWNER, &["100%", "picnic_day"]).await,
        [("group:qq:G".to_owned(), 0)]
    );
    assert!(
        words(&OWNER, &["100x"]).await.is_empty(),
        "% is not a wildcard"
    );
    assert!(words(&OWNER, &["picnic?day"]).await.is_empty());
    let only_group = RecallScope::OnlyKey("group:qq:G".into());
    assert!(
        store
            .recall_words(&only_group, current.id, &["router".to_owned()], 20)
            .await
            .unwrap()
            .is_empty()
    );

    // Vectors: both entries are missing, then stored, then ranked.
    let missing = store.recall_missing(&OWNER, "m", 10).await.unwrap();
    assert_eq!(missing.len(), 2);
    let telegram = EntryRef {
        session_id: 1,
        seq: 0,
    };
    let picnic = EntryRef {
        session_id: group.id.0,
        seq: 0,
    };
    store
        .recall_put("m", &[(telegram, axis(0)), (picnic, axis(1))])
        .await
        .unwrap();
    assert!(
        store
            .recall_missing(&OWNER, "m", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .recall_missing(&OWNER, "other", 10)
            .await
            .unwrap()
            .len(),
        2
    );
    let nearest = store
        .recall_nearest(&OWNER, current.id, "m", &axis(1), 5)
        .await
        .unwrap();
    assert_eq!(
        nearest.iter().map(|hit| hit.entry).collect::<Vec<_>>(),
        [picnic, telegram]
    );
    let nearest = store
        .recall_nearest(&only_group, current.id, "m", &axis(0), 5)
        .await
        .unwrap();
    assert_eq!(
        nearest.iter().map(|hit| hit.entry).collect::<Vec<_>>(),
        [picnic]
    );

    // The searching session's visible entries are left out.
    store
        .append(
            current.id,
            0,
            &HistoryEntry::User(UserEntry::text_only(
                MessageId(1),
                MessageOrigin::Owner,
                "[clock]\nrouter again",
            )),
        )
        .await
        .unwrap();
    assert_eq!(words(&OWNER, &["router"]).await.len(), 1);
    store
        .save_checkpoint(current.id, 1, "summary")
        .await
        .unwrap();
    assert_eq!(
        words(&OWNER, &["router"]).await,
        [("owner:qq".to_owned(), 0), ("owner:telegram".to_owned(), 0)]
    );

    // Around an entry, within scope only.
    let around = store
        .recall_around(&OWNER, telegram, 3, 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(around.len(), 1);
    assert_eq!(around[0].text, "My Router dropped the 路由 link");
    assert!(
        store
            .recall_around(&only_group, telegram, 3, 3)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .recall_around(
                &OWNER,
                EntryRef {
                    session_id: 1,
                    seq: 9
                },
                3,
                3
            )
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn the_waiting_queue_is_kept_until_taken() {
    let Some(cluster) = Cluster::start() else {
        return;
    };
    cluster.create_database("pending");
    let store = PgStore::connect(&cluster.target("pending"), Path::new(NO_FOLDER))
        .await
        .unwrap();
    let session = store.create("owner:qq", Path::new("/")).await.unwrap();
    let at = chrono::Utc::now();
    for (id, text) in [(3, "third"), (1, "first"), (2, "second")] {
        let row = PendingRow {
            message_id: id,
            submitted_at: at,
            text: text.into(),
        };
        store.add_pending(session.id, &row).await.unwrap();
    }
    store.remove_pending(session.id, 2).await.unwrap();
    let taken = store.take_pending(session.id).await.unwrap();
    assert_eq!(
        taken
            .iter()
            .map(|row| (row.message_id, row.text.as_str()))
            .collect::<Vec<_>>(),
        [(1, "first"), (3, "third")]
    );
    assert!(store.take_pending(session.id).await.unwrap().is_empty());
}
