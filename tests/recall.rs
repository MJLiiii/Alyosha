//! Recall against the in-memory session store and a fake embedder.
//! See `docs/Recall.md#verification`.

use std::{
    collections::HashMap,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use alyosha::{
    BoxFuture,
    agent_state::{
        AssistantEntry, AssistantPart, HistoryEntry, MessageId, MessageOrigin, ToolCall,
        ToolOutcome, ToolResultEntry, UserEntry,
    },
    recall::{EMBED_BATCH, EMBED_PER_SEARCH, Embedder, EntryRef, Recall, RecallScope},
    session_store::{MemoryStore, SessionId, SessionStore},
};
use serde_json::json;

/// Gives "dog" and "puppy" the same direction, so a search for "puppy"
/// finds a dog by meaning only.
#[derive(Default)]
struct FakeEmbedder {
    failing: AtomicBool,
    /// The size of each request, in order.
    calls: Mutex<Vec<usize>>,
}

impl Embedder for FakeEmbedder {
    fn embed<'a>(&'a self, texts: &'a [String]) -> BoxFuture<'a, Result<Vec<Vec<f32>>, String>> {
        Box::pin(async move {
            if self.failing.load(Ordering::SeqCst) {
                return Err("the fake server is down".into());
            }
            self.calls.lock().unwrap().push(texts.len());
            Ok(texts
                .iter()
                .map(|text| {
                    let text = text.to_lowercase();
                    let dog = (text.contains("dog") || text.contains("puppy")) as u8 as f32;
                    let cat = text.contains("cat") as u8 as f32;
                    vec![dog, cat, 0.01]
                })
                .collect())
        })
    }
}

fn message(id: u64, text: &str) -> HistoryEntry {
    HistoryEntry::User(UserEntry::text_only(
        MessageId(id),
        MessageOrigin::Owner,
        format!("[Wed 2026-11-25 10:02, Europe/Amsterdam]\n{text}"),
    ))
}

fn reply(text: &str) -> HistoryEntry {
    HistoryEntry::Assistant(AssistantEntry {
        parts: vec![AssistantPart::Text(text.into())],
    })
}

/// Creates a session under `key` holding `entries`, closed.
async fn session(store: &MemoryStore, key: &str, entries: &[HistoryEntry]) -> SessionId {
    let stored = store.create(key, Path::new("/")).await.unwrap();
    for (seq, entry) in entries.iter().enumerate() {
        store.append(stored.id, seq as u64, entry).await.unwrap();
    }
    store.close(stored.id).await.unwrap();
    stored.id
}

fn recall(
    store: &Arc<MemoryStore>,
    current: SessionId,
    key: &str,
    embedder: Option<Arc<FakeEmbedder>>,
) -> Recall {
    Recall {
        store: store.clone(),
        session: current,
        scope: RecallScope::for_key(key),
        embedder: embedder.map(|embedder| embedder as Arc<dyn Embedder>),
        model: "fake".into(),
        timezone: chrono_tz::Europe::Amsterdam,
        labels: HashMap::from([
            ("owner:telegram".into(), "your private Telegram chat".into()),
            ("group:qq:G".into(), "the trusted QQ group friends".into()),
        ]),
    }
}

fn refs(output: &str) -> Vec<String> {
    output
        .lines()
        .filter(|line| !line.starts_with(' ') && line.contains(" · "))
        .map(|line| line.split(" · ").next().unwrap().to_owned())
        .collect()
}

#[tokio::test]
async fn an_earlier_session_is_found_and_read_around() {
    let store = MemoryStore::new();
    let old = session(
        &store,
        "owner:telegram",
        &[
            message(1, "My router keeps dropping Wi-Fi"),
            reply("Try channel 6."),
            message(2, "That worked"),
        ],
    )
    .await;
    let current = session(&store, "owner:telegram", &[message(1, "hello again")]).await;
    let recall = recall(&store, current, "owner:telegram", None);

    let found = recall.search("router").await.unwrap();
    assert_eq!(refs(&found), [format!("{}:0", old.0)], "{found}");
    assert!(
        found.contains("your private Telegram chat, session from"),
        "{found}"
    );
    assert!(
        found.contains("· message\n  My router keeps dropping Wi-Fi"),
        "{found}"
    );
    assert!(
        !found.contains("[Wed"),
        "the clock line is not searched or shown"
    );

    let around = recall
        .read(
            EntryRef {
                session_id: old.0,
                seq: 1,
            },
            1,
            1,
        )
        .await
        .unwrap();
    assert!(around.starts_with(&format!("Session {} in your private Telegram chat", old.0)));
    for (seq, text) in [(0, "router"), (1, "Try channel 6."), (2, "That worked")] {
        assert!(around.contains(&format!("--- {}:{seq}", old.0)), "{around}");
        assert!(around.contains(text), "{around}");
    }
    assert!(around.contains(&format!("{}:1 ·", old.0)) && around.contains("reply (the hit)"));
    assert_eq!(
        recall.search("   ").await.unwrap_err(),
        "The query is empty."
    );
}

#[tokio::test]
async fn only_what_the_session_no_longer_sees_is_searched() {
    let store = MemoryStore::new();
    let stored = store
        .create("owner:telegram", Path::new("/"))
        .await
        .unwrap();
    let entries = [
        message(1, "summarized away: invoice 4411"),
        reply("noted"),
        message(2, "still visible: invoice 9922"),
        reply("ok"),
    ];
    for (seq, entry) in entries.iter().enumerate() {
        store.append(stored.id, seq as u64, entry).await.unwrap();
    }
    let recall = recall(&store, stored.id, "owner:telegram", None);
    // Before any compaction the model sees all of it.
    assert_eq!(recall.search("invoice").await.unwrap(), "Nothing found.");
    store
        .save_checkpoint(stored.id, 2, "summary")
        .await
        .unwrap();
    let found = recall.search("invoice").await.unwrap();
    assert_eq!(refs(&found), [format!("{}:0", stored.id.0)], "{found}");
}

#[tokio::test]
async fn chinese_words_and_mixed_case_are_found_and_every_term_must_appear() {
    let store = MemoryStore::new();
    let old = session(
        &store,
        "owner:qq",
        &[
            message(1, "我换了路由器，网络好了"),
            message(2, "The NixOS Container works"),
        ],
    )
    .await;
    let current = session(&store, "owner:qq", &[]).await;
    let recall = recall(&store, current, "owner:qq", None);
    assert_eq!(
        refs(&recall.search("路由").await.unwrap()),
        [format!("{}:0", old.0)]
    );
    assert_eq!(
        refs(&recall.search("nixos CONTAINER").await.unwrap()),
        [format!("{}:1", old.0)]
    );
    assert_eq!(recall.search("nixos 路由").await.unwrap(), "Nothing found.");
}

#[tokio::test]
async fn private_chats_search_groups_but_groups_search_only_themselves() {
    let store = MemoryStore::new();
    let private = session(&store, "owner:telegram", &[message(1, "secret plan alpha")]).await;
    let group = session(&store, "group:qq:G", &[message(1, "group picnic beta")]).await;
    let guest = session(&store, "guest:qq:H", &[message(1, "guest question gamma")]).await;
    let owner_now = session(&store, "owner:qq", &[]).await;

    let from_owner = recall(&store, owner_now, "owner:qq", None);
    let found = from_owner.search("picnic").await.unwrap();
    assert_eq!(refs(&found), [format!("{}:0", group.0)]);
    assert!(found.contains("the trusted QQ group friends"), "{found}");
    assert!(
        from_owner
            .search("alpha")
            .await
            .unwrap()
            .contains(&format!("{}:0", private.0))
    );
    assert_eq!(from_owner.search("gamma").await.unwrap(), "Nothing found.");

    let private_ref = EntryRef {
        session_id: private.0,
        seq: 0,
    };
    for (current, key, own) in [
        (group, "group:qq:G", "beta"),
        (guest, "guest:qq:H", "gamma"),
    ] {
        let here = recall(&store, SessionId(9_999), key, None);
        assert_eq!(here.search("alpha").await.unwrap(), "Nothing found.");
        assert_eq!(
            here.read(private_ref, 3, 3).await.unwrap_err(),
            format!("No entry {private_ref}.")
        );
        let found = here.search(own).await.unwrap();
        assert_eq!(refs(&found), [format!("{}:0", current.0)], "{key}");
    }
    let guest_found = recall(&store, SessionId(9_999), "guest:qq:H", None)
        .search("gamma")
        .await
        .unwrap();
    assert!(guest_found.contains("this group"), "{guest_found}");
    assert_eq!(
        from_owner
            .read(
                EntryRef {
                    session_id: private.0,
                    seq: 7
                },
                3,
                3
            )
            .await
            .unwrap_err(),
        format!("No entry {}:7.", private.0)
    );
}

#[tokio::test]
async fn tool_calls_and_results_are_searched() {
    let store = MemoryStore::new();
    let old = session(
        &store,
        "owner:telegram",
        &[
            message(1, "read my mail"),
            HistoryEntry::Assistant(AssistantEntry {
                parts: vec![AssistantPart::ToolCall(ToolCall {
                    id: "c1".into(),
                    name: "gmail_search".into(),
                    arguments: json!({"query": "from:anna@example.org"}),
                })],
            }),
            HistoryEntry::ToolResult(ToolResultEntry {
                call_id: "c1".into(),
                outcome: ToolOutcome::Success("Anna: meet at Kalverstraat 12".into()),
            }),
        ],
    )
    .await;
    let current = session(&store, "owner:telegram", &[]).await;
    let recall = recall(&store, current, "owner:telegram", None);
    let found = recall.search("anna@example.org").await.unwrap();
    assert_eq!(refs(&found), [format!("{}:1", old.0)], "{found}");
    assert!(found.contains("gmail_search"), "{found}");
    let found = recall.search("Kalverstraat").await.unwrap();
    assert_eq!(refs(&found), [format!("{}:2", old.0)]);
    assert!(found.contains("· tool result"), "{found}");
}

#[tokio::test]
async fn meaning_finds_what_the_words_miss_and_failures_fall_back() {
    let store = MemoryStore::new();
    let old = session(
        &store,
        "owner:telegram",
        &[
            message(1, "my dog barked all night"),
            message(2, "the cat slept"),
        ],
    )
    .await;
    let current = session(&store, "owner:telegram", &[]).await;
    let embedder = Arc::new(FakeEmbedder::default());
    embedder.failing.store(true, Ordering::SeqCst);
    let recall = recall(&store, current, "owner:telegram", Some(embedder.clone()));

    let found = recall.search("puppy").await.unwrap();
    assert_eq!(
        found,
        "Meaning search unavailable: the fake server is down.\nNothing found."
    );
    assert_eq!(store.vector_count("fake"), 0);

    embedder.failing.store(false, Ordering::SeqCst);
    let found = recall.search("puppy").await.unwrap();
    assert_eq!(
        store.vector_count("fake"),
        2,
        "the missing entries were embedded"
    );
    assert_eq!(refs(&found)[0], format!("{}:0", old.0), "{found}");
    // One batch for the entries, one for the query.
    assert_eq!(*embedder.calls.lock().unwrap(), [2, 1]);

    // A hit by both ways ranks above one by meaning only.
    let found = recall.search("cat").await.unwrap();
    assert_eq!(refs(&found)[0], format!("{}:1", old.0), "{found}");
    assert_eq!(
        *embedder.calls.lock().unwrap(),
        [2, 1, 1],
        "nothing left to embed"
    );
}

#[tokio::test]
async fn one_search_embeds_at_most_its_share_in_batches() {
    let store = MemoryStore::new();
    let entries: Vec<HistoryEntry> = (0..2_500)
        .map(|i| message(i, &format!("note {i}")))
        .collect();
    session(&store, "owner:telegram", &entries).await;
    let current = session(&store, "owner:telegram", &[]).await;
    let embedder = Arc::new(FakeEmbedder::default());
    let recall = recall(&store, current, "owner:telegram", Some(embedder.clone()));

    recall.search("note").await.unwrap();
    assert_eq!(store.vector_count("fake"), EMBED_PER_SEARCH);
    let calls = embedder.calls.lock().unwrap().clone();
    assert_eq!(calls.len(), EMBED_PER_SEARCH / EMBED_BATCH + 1);
    assert!(
        calls[..calls.len() - 1]
            .iter()
            .all(|size| *size == EMBED_BATCH)
    );

    recall.search("note").await.unwrap();
    assert_eq!(store.vector_count("fake"), 2_500);

    // A new model gets vectors of its own.
    let mut other = recall;
    other.model = "fake-2".into();
    other.search("note").await.unwrap();
    assert_eq!(store.vector_count("fake-2"), EMBED_PER_SEARCH);
    assert_eq!(store.vector_count("fake"), 2_500);
}
