//! The sessions of group chats, one child per group, started at the
//! group's first mention. A trusted group gets `alyosha acp --group` in its
//! own folder and keeps it until the bridge stops; any other group gets
//! `alyosha acp --guest`, stopped after an idle hour. See
//! `docs/Channels.md#group-chats` and
//! `docs/TelegramBridge.md#group-chats`.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::sync::mpsc;

use crate::{Bridge, Event};

/// How long a group may have no open prompt before its guest stops.
pub const IDLE_LIMIT: Duration = Duration::from_secs(60 * 60);

/// Starts one group child: `alyosha acp --guest` or `--group`, or a fake
/// in tests.
pub type SpawnGuest =
    Arc<dyn Fn() -> std::io::Result<(Bridge, mpsc::UnboundedReceiver<Event>)> + Send + Sync>;

/// An event from a group's guest child. `guest` tells a stopped child's
/// late events from those of a newer child for the same group.
#[derive(Debug)]
pub struct GuestEvent {
    pub group: String,
    pub guest: u64,
    pub event: Event,
}

struct Guest {
    number: u64,
    bridge: Bridge,
    last_active: Instant,
    trusted: bool,
}

/// A trusted group's command, as `GuestPool::group_command` reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupCommand {
    /// `/new`: a new session for the group.
    New,
    /// `/cancel`: stop the group's turn and its open requests.
    Cancel,
    /// `/compact` or `/job …`: sent to the group's session as it is.
    Session,
}

/// How a bridge starts its group children.
#[derive(Clone, Default)]
pub struct GroupSpawners {
    /// `alyosha acp --guest`; `None` turns guest groups off.
    pub guest: Option<SpawnGuest>,
    /// `alyosha acp --group`; `None` turns trusted groups off.
    pub trusted: Option<SpawnGuest>,
    /// The trusted groups: ID → name. The name is the group's working folder.
    pub trusted_groups: HashMap<String, String>,
}

/// The group children, keyed by group.
pub struct GuestPool {
    spawn: GroupSpawners,
    client_name: String,
    /// Alyosha's working folders: `groups/<name>` and `guest` are under it.
    work_dir: PathBuf,
    guests: HashMap<String, Guest>,
    next: u64,
    events: mpsc::UnboundedSender<GuestEvent>,
}

impl GuestPool {
    /// Without a spawner, that kind of group is off and its prompts fail.
    pub fn new(
        spawn: GroupSpawners,
        client_name: &str,
        work_dir: PathBuf,
    ) -> (Self, mpsc::UnboundedReceiver<GuestEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Self {
                spawn,
                client_name: client_name.to_owned(),
                work_dir,
                guests: HashMap::new(),
                next: 1,
                events,
            },
            receiver,
        )
    }

    /// Whether `group` is a trusted group.
    pub fn is_trusted(&self, group: &str) -> bool {
        self.spawn.trusted_groups.contains_key(group)
    }

    /// A trusted group's name in the settings, or its ID for any other group.
    pub fn name<'a>(&'a self, group: &'a str) -> &'a str {
        self.spawn
            .trusted_groups
            .get(group)
            .map_or(group, String::as_str)
    }

    /// The ID of the trusted group under `name` in the settings.
    pub fn trusted_named(&self, name: &str) -> Option<String> {
        self.spawn
            .trusted_groups
            .iter()
            .find(|(_, group_name)| group_name.as_str() == name)
            .map(|(group, _)| group.clone())
    }

    /// Whether `text` is one of a trusted group's commands: `/new`,
    /// `/cancel`, `/compact`, or `/job …`. See
    /// `docs/Channels.md#commands-in-trusted-groups`.
    pub fn group_command(text: &str) -> Option<GroupCommand> {
        match text.trim() {
            "/new" => Some(GroupCommand::New),
            "/cancel" => Some(GroupCommand::Cancel),
            "/compact" => Some(GroupCommand::Session),
            text if text == "/job" || text.starts_with("/job ") => Some(GroupCommand::Session),
            _ => None,
        }
    }

    /// Starts a new session for a trusted group in its folder, starting its
    /// child first if none runs.
    pub async fn new_session(&mut self, group: &str) -> Result<(), String> {
        self.ensure(group).await?;
        let workspace = self.workspace(group);
        let key = self.session_key(group);
        let guest = self.guests.get_mut(group).expect("started above");
        guest.last_active = Instant::now();
        guest.bridge.new_session(&workspace, &key).await.map(drop)
    }

    /// `session/cancel` for a group's running session, then each of
    /// `requests` answered `cancelled`. Returns false if none runs.
    pub fn cancel(&self, group: &str, requests: &[u64]) -> bool {
        match self.guests.get(group) {
            Some(guest) => {
                guest.bridge.cancel_with_requests(requests);
                true
            }
            None => false,
        }
    }

    /// Sends a command such as `/compact` to a trusted group's session as
    /// it is, without the sender's name, starting the child if needed.
    pub async fn command(&mut self, group: &str, key: &str, text: &str) -> Result<(), String> {
        self.ensure(group).await?;
        let guest = self.guests.get_mut(group).expect("started above");
        guest.last_active = Instant::now();
        guest.bridge.prompt(key, text.trim());
        Ok(())
    }

    async fn ensure(&mut self, group: &str) -> Result<(), String> {
        if !self.guests.contains_key(group) {
            let guest = self.start(group).await?;
            self.guests.insert(group.to_owned(), guest);
        }
        Ok(())
    }

    /// A trusted group works in the folder named after it; a guest session
    /// ignores its workspace, which is the guest folder.
    fn workspace(&self, group: &str) -> PathBuf {
        if self.is_trusted(group) {
            self.work_dir.join("groups").join(self.name(group))
        } else {
            self.work_dir.join("guest")
        }
    }

    /// `alyosha-telegram` saves its groups' sessions as
    /// `group:telegram:<id>` or `guest:telegram:<id>`.
    fn session_key(&self, group: &str) -> String {
        let platform = self
            .client_name
            .strip_prefix("alyosha-")
            .unwrap_or(&self.client_name);
        let kind = if self.is_trusted(group) {
            "group"
        } else {
            "guest"
        };
        format!("{kind}:{platform}:{group}")
    }

    /// The running child of `group`, to answer its requests.
    pub fn bridge(&self, group: &str) -> Option<&Bridge> {
        self.guests.get(group).map(|guest| &guest.bridge)
    }

    /// Sends a group member's message to the group's session, starting it
    /// first if none runs, with the sender in `_meta`. Only a trusted
    /// group's session is told that the sender is the owner.
    #[allow(clippy::too_many_arguments)]
    pub async fn prompt(
        &mut self,
        group: &str,
        key: &str,
        text: &str,
        blocks: Vec<serde_json::Value>,
        sender_id: &str,
        sender_name: &str,
        owner: bool,
    ) -> Result<(), String> {
        self.ensure(group).await?;
        let guest = self.guests.get_mut(group).expect("inserted above");
        guest.last_active = Instant::now();
        let owner = owner && guest.trusted;
        guest
            .bridge
            .prompt_from(key, text, blocks, sender_id, sender_name, owner);
        Ok(())
    }

    /// Gives a finished job's result to a running trusted group's session.
    /// Returns false if the group has no running session.
    pub fn prompt_job(&mut self, group: &str, key: &str, text: &str) -> bool {
        let Some(guest) = self.guests.get_mut(group) else {
            return false;
        };
        guest.last_active = Instant::now();
        guest.bridge.prompt(key, text);
        true
    }

    async fn start(&mut self, group: &str) -> Result<Guest, String> {
        let trusted = self.is_trusted(group);
        let spawn = if trusted {
            self.spawn
                .trusted
                .as_ref()
                .ok_or("trusted groups are off")?
        } else {
            self.spawn.guest.as_ref().ok_or("group chats are off")?
        };
        let workspace = self.workspace(group);
        std::fs::create_dir_all(&workspace).map_err(|error| error.to_string())?;
        let (bridge, mut events) = spawn().map_err(|error| error.to_string())?;
        let number = self.next;
        self.next += 1;
        let forward = self.events.clone();
        let group_name = group.to_owned();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                let event = GuestEvent {
                    group: group_name.clone(),
                    guest: number,
                    event,
                };
                if forward.send(event).is_err() {
                    return;
                }
            }
        });
        let key = self.session_key(group);
        let started = async {
            bridge.initialize(&self.client_name).await?;
            bridge.open_session(&workspace, &key).await
        }
        .await;
        if let Err(error) = started {
            bridge.shutdown().await;
            return Err(error);
        }
        tracing::info!(group, trusted, "started a group session");
        Ok(Guest {
            number,
            bridge,
            last_active: Instant::now(),
            trusted,
        })
    }

    /// Marks the group active, as when a reply arrives.
    pub fn touch(&mut self, group: &str) {
        if let Some(guest) = self.guests.get_mut(group) {
            guest.last_active = Instant::now();
        }
    }

    /// Groups whose guest has an open prompt.
    pub fn busy_groups(&self) -> Vec<String> {
        self.guests
            .iter()
            .filter(|(_, guest)| !guest.bridge.open_keys().is_empty())
            .map(|(group, _)| group.clone())
            .collect()
    }

    /// Forgets a guest that exited, unless a newer one runs for the group.
    pub fn exited(&mut self, group: &str, guest: u64) {
        if self
            .guests
            .get(group)
            .is_some_and(|running| running.number == guest)
        {
            tracing::warn!(group, "a guest session exited");
            self.guests.remove(group);
        }
    }

    /// Stops the guests of groups with no open prompt for `idle`. Trusted
    /// groups' sessions keep running, with their jobs.
    pub async fn stop_idle(&mut self, idle: Duration) {
        let quiet: Vec<String> = self
            .guests
            .iter()
            .filter(|(_, guest)| {
                !guest.trusted
                    && guest.bridge.open_keys().is_empty()
                    && guest.last_active.elapsed() >= idle
            })
            .map(|(group, _)| group.clone())
            .collect();
        for group in quiet {
            if let Some(guest) = self.guests.remove(&group) {
                tracing::info!(group = %group, "stopped an idle guest session");
                guest.bridge.shutdown().await;
            }
        }
    }

    /// Stops every guest, at the bridge's shutdown.
    pub async fn shutdown(&mut self) {
        for (_, guest) in self.guests.drain() {
            guest.bridge.shutdown().await;
        }
    }

    /// The number of running guests.
    pub fn len(&self) -> usize {
        self.guests.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guests.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    use super::*;

    #[test]
    fn only_whole_commands_count() {
        assert_eq!(GuestPool::group_command(" /new "), Some(GroupCommand::New));
        assert_eq!(
            GuestPool::group_command("/cancel"),
            Some(GroupCommand::Cancel)
        );
        assert_eq!(
            GuestPool::group_command("/compact"),
            Some(GroupCommand::Session)
        );
        assert_eq!(
            GuestPool::group_command("/job make test"),
            Some(GroupCommand::Session)
        );
        for text in ["/new please", "please /compact", "/jobs", "/newer", "hello"] {
            assert_eq!(GuestPool::group_command(text), None, "{text}");
        }
    }

    /// An agent that answers `initialize`, `session/load` with not found,
    /// and `session/new`, and logs what it receives.
    fn spawner(log: Arc<Mutex<Vec<Value>>>) -> SpawnGuest {
        Arc::new(move || {
            let (ours, theirs) = tokio::io::duplex(1 << 16);
            let (read, mut write) = tokio::io::split(ours);
            let (bridge_read, bridge_write) = tokio::io::split(theirs);
            let log = log.clone();
            tokio::spawn(async move {
                let mut lines = BufReader::new(read).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let message: Value = serde_json::from_str(&line).unwrap();
                    log.lock().unwrap().push(message.clone());
                    let id = message["id"].clone();
                    let reply = match message["method"].as_str() {
                        Some("initialize") => {
                            json!({"jsonrpc": "2.0", "id": id, "result": {"protocolVersion": 1}})
                        }
                        Some("session/load") => {
                            json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32002, "message": "not found"}})
                        }
                        Some("session/new") => {
                            json!({"jsonrpc": "2.0", "id": id, "result": {"sessionId": "s1"}})
                        }
                        _ => continue,
                    };
                    write
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
            });
            Ok(Bridge::connect(bridge_read, bridge_write))
        })
    }

    #[tokio::test]
    async fn trusted_groups_start_new_sessions_cancel_and_take_commands() {
        let root = tempfile::tempdir().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let (mut pool, _events) = GuestPool::new(
            GroupSpawners {
                guest: None,
                trusted: Some(spawner(log.clone())),
                trusted_groups: [("G1".to_owned(), "dev".to_owned())].into(),
            },
            "alyosha-qq",
            root.path().to_owned(),
        );
        assert_eq!(pool.trusted_named("dev").as_deref(), Some("G1"));
        assert_eq!(pool.trusted_named("nosuch"), None);
        assert!(!pool.cancel("G1", &[]), "nothing runs yet");

        pool.new_session("G1").await.unwrap();
        pool.command("G1", "k1", " /compact ").await.unwrap();
        assert!(pool.cancel("G1", &[42]));
        // Let the writes reach the agent.
        for _ in 0..100 {
            if log.lock().unwrap().len() >= 7 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let log = log.lock().unwrap().clone();
        let methods: Vec<&str> = log
            .iter()
            .map(|message| message["method"].as_str().unwrap_or("answer"))
            .collect();
        assert_eq!(
            methods,
            [
                "initialize",
                "session/load",
                "session/new",
                "session/new",
                "session/prompt",
                "session/cancel",
                "answer"
            ]
        );
        let folder = root.path().join("groups").join("dev");
        assert_eq!(log[3]["params"]["cwd"], folder.display().to_string());
        assert_eq!(
            log[3]["params"]["_meta"]["alyosha"]["sessionKey"],
            "group:qq:G1"
        );
        assert_eq!(log[4]["params"]["prompt"][0]["text"], "/compact");
        assert_eq!(log[6]["id"], 42);
        assert_eq!(log[6]["result"]["outcome"]["outcome"], "cancelled");
    }
}
