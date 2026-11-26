//! `PgStore` against a throwaway PostgreSQL cluster, started with `initdb`
//! and `pg_ctl` in a temporary folder. Without them on `PATH`, the test
//! prints a skip notice and passes. See
//! `docs/SessionStore.md#verification`.

use std::{
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};

use alyosha::{
    agent_state::{
        AssistantEntry, AssistantPart, HistoryEntry, MessageId, MessageOrigin, Replay, ToolCall,
        ToolOutcome, ToolResultEntry, UserEntry,
    },
    session_store::{PgStore, SessionStore, StoreError},
};
use serde_json::json;

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
        Some(Self {
            _root: root,
            data,
            socket,
        })
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
        HistoryEntry::User(UserEntry {
            message_id: MessageId(3),
            origin: MessageOrigin::Owner,
            text: "[clock]\nread a".into(),
        }),
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
    let (first, second) = tokio::join!(PgStore::connect(&target), PgStore::connect(&target));
    first.unwrap();
    second.unwrap();
    assert_eq!(
        cluster.psql(
            "migrate",
            "select string_agg(version::text, ',') from schema_version"
        ),
        "1"
    );
    cluster.psql("migrate", "insert into schema_version values (99)");
    let Err(StoreError::Failed(error)) = PgStore::connect(&cluster.target("migrate")).await else {
        panic!("a newer database is refused");
    };
    assert!(error.contains("newer"), "{error}");

    // Append, checkpoint, and resume.
    cluster.create_database("alyosha");
    let store = PgStore::connect(&cluster.target("alyosha")).await.unwrap();
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
    let other = PgStore::connect(&cluster.target("alyosha")).await.unwrap();
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
