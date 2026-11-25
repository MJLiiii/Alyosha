//! The guest sessions of group chats: one `alyosha acp --guest` child per
//! group, started at the group's first mention and stopped after an idle
//! hour. See `docs/design/Channels.md#group-chats` and
//! `docs/develop/TelegramBridge.md#group-chats`.

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

/// Starts one guest child: `alyosha acp --guest`, or a fake in tests.
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
}

/// The guest children, keyed by group.
pub struct GuestPool {
    spawn: Option<SpawnGuest>,
    client_name: String,
    workspace: PathBuf,
    guests: HashMap<String, Guest>,
    next: u64,
    events: mpsc::UnboundedSender<GuestEvent>,
}

impl GuestPool {
    /// Without `spawn`, group chats are off and every prompt fails.
    pub fn new(
        spawn: Option<SpawnGuest>,
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

    /// Sends a group member's message to the group's guest session,
    /// starting it first if none runs.
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
        guest.bridge.prompt_from(key, text, sender_id, sender_name);
        Ok(())
    }

    async fn start(&mut self, group: &str) -> Result<Guest, String> {
        let spawn = self.spawn.as_ref().ok_or("group chats are off")?;
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
        let started = async {
            bridge.initialize(&self.client_name).await?;
            bridge.new_session(&self.workspace).await
        }
        .await;
        if let Err(error) = started {
            bridge.shutdown().await;
            return Err(error);
        }
        tracing::info!(group, "started a guest session");
        Ok(Guest {
            number,
            bridge,
            last_active: Instant::now(),
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

    /// Stops the guests of groups with no open prompt for `idle`.
    pub async fn stop_idle(&mut self, idle: Duration) {
        let quiet: Vec<String> = self
            .guests
            .iter()
            .filter(|(_, guest)| {
                guest.bridge.open_keys().is_empty() && guest.last_active.elapsed() >= idle
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
