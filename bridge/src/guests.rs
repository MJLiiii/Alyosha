//! The sessions of group chats, one child per group, started at the
//! group's first mention. A trusted group gets `alyosha acp --group` in its
//! own folder and keeps it until the bridge stops; any other group gets
//! `alyosha acp --guest`, stopped after an idle hour. See
//! `docs/Channels.md#group-chats` and
//! `docs/TelegramBridge.md#group-chats`.

use std::{
    collections::{HashMap, HashSet},
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

/// How a bridge starts its group children.
#[derive(Clone, Default)]
pub struct GroupSpawners {
    /// `alyosha acp --guest`; `None` turns guest groups off.
    pub guest: Option<SpawnGuest>,
    /// `alyosha acp --group`; `None` turns trusted groups off.
    pub trusted: Option<SpawnGuest>,
    /// The trusted groups' IDs.
    pub trusted_groups: HashSet<String>,
}

/// The group children, keyed by group.
pub struct GuestPool {
    spawn: GroupSpawners,
    client_name: String,
    workspace: PathBuf,
    guests: HashMap<String, Guest>,
    next: u64,
    events: mpsc::UnboundedSender<GuestEvent>,
}

impl GuestPool {
    /// Without a spawner, that kind of group is off and its prompts fail.
    pub fn new(
        spawn: GroupSpawners,
        client_name: &str,
        workspace: PathBuf,
    ) -> (Self, mpsc::UnboundedReceiver<GuestEvent>) {
        let (events, receiver) = mpsc::unbounded_channel();
        (
            Self {
                spawn,
                client_name: client_name.to_owned(),
                workspace,
                guests: HashMap::new(),
                next: 1,
                events,
            },
            receiver,
        )
    }

    /// Whether `group` is a trusted group.
    pub fn is_trusted(&self, group: &str) -> bool {
        self.spawn.trusted_groups.contains(group)
    }

    /// The running child of `group`, to answer its requests.
    pub fn bridge(&self, group: &str) -> Option<&Bridge> {
        self.guests.get(group).map(|guest| &guest.bridge)
    }

    /// Sends a group member's message to the group's session, starting it
    /// first if none runs. A trusted group's session gets `<name>: <text>`;
    /// a guest session gets the sender in `_meta`.
    pub async fn prompt(
        &mut self,
        group: &str,
        key: &str,
        text: &str,
        sender_id: &str,
        sender_name: &str,
    ) -> Result<(), String> {
        if !self.guests.contains_key(group) {
            let guest = self.start(group).await?;
            self.guests.insert(group.to_owned(), guest);
        }
        let guest = self.guests.get_mut(group).expect("inserted above");
        guest.last_active = Instant::now();
        if guest.trusted {
            guest.bridge.prompt(key, &format!("{sender_name}: {text}"));
        } else {
            guest.bridge.prompt_from(key, text, sender_id, sender_name);
        }
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
        // A trusted group works in a folder of its own; a guest session
        // ignores its workspace.
        let workspace = if trusted {
            let folder = self.workspace.join("groups").join(safe_name(group));
            std::fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
            folder
        } else {
            self.workspace.clone()
        };
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
        // `alyosha-telegram` saves its groups' sessions as
        // `group:telegram:<id>` or `guest:telegram:<id>`.
        let platform = self
            .client_name
            .strip_prefix("alyosha-")
            .unwrap_or(&self.client_name);
        let key = format!(
            "{}:{platform}:{group}",
            if trusted { "group" } else { "guest" }
        );
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

/// A group ID as a folder name: QQ's `group_openid`s and Telegram's chat
/// IDs are already safe, but nothing else gets through.
fn safe_name(group: &str) -> String {
    let name: String = group
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if name.is_empty() {
        "_".to_owned()
    } else {
        name
    }
}
