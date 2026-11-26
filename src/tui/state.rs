//! The TUI's state, updated by a pure function of the previous state and one
//! key or session event, so tests can drive it without a terminal.

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::{
    agent_loop::{JOB_USAGE, SessionEvent, Status},
    agent_state::{HistoryEntry, MessageId, MessageOrigin, TurnOutcome},
    approvals::{OwnerAnswer, OwnerRequest},
};

/// The longest the input box grows before it scrolls.
pub const MAX_INPUT_LINES: usize = 8;
const DOUBLE_PRESS: Duration = Duration::from_secs(2);
const QUIT_AGAIN: Duration = Duration::from_secs(5);

pub const HELP: &str = "Enter send · Alt+Enter or Ctrl+J new line · Up/Down recall · Esc stop · Ctrl+C stop or clear (twice to quit) · Ctrl+D or /quit quit\n/compact summarize older conversation · /job <command> start a background job · /help this list";

/// How a scrollback line looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineStyle {
    Plain,
    Dim,
    Warning,
}

/// A line to print above the viewport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrintLine {
    pub text: String,
    pub style: LineStyle,
}

impl PrintLine {
    fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: LineStyle::Plain,
        }
    }
    fn dim(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: LineStyle::Dim,
        }
    }
    fn warning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: LineStyle::Warning,
        }
    }
}

/// Your last message and the reply after it, as the scrollback showed them,
/// for a resumed session.
pub fn last_exchange(history: &[HistoryEntry]) -> Vec<PrintLine> {
    let Some(start) = history.iter().rposition(
        |entry| matches!(entry, HistoryEntry::User(user) if user.origin == MessageOrigin::Owner),
    ) else {
        return Vec::new();
    };
    let HistoryEntry::User(user) = &history[start] else {
        unreachable!("found above");
    };
    let text = user.text.split_once('\n').map_or("", |(_, rest)| rest);
    let mut lines: Vec<PrintLine> = text
        .lines()
        .enumerate()
        .map(|(index, line)| {
            let prefix = if index == 0 { "› " } else { "  " };
            PrintLine::plain(format!("{prefix}{line}"))
        })
        .collect();
    let reply = history[start + 1..]
        .iter()
        .rev()
        .find_map(|entry| match entry {
            HistoryEntry::Assistant(reply) if !reply.text().trim().is_empty() => Some(reply.text()),
            _ => None,
        });
    if let Some(reply) = reply {
        lines.push(PrintLine::plain(""));
        lines.extend(reply.lines().map(PrintLine::plain));
    }
    lines
}

/// What the TUI asks of the session or the terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Print(Vec<PrintLine>),
    Submit(String),
    Stop,
    Compact,
    StartJob(String),
    Answer(OwnerAnswer),
    Quit,
}

/// An editable buffer with a cursor at a byte index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
}

impl Editor {
    pub fn insert(&mut self, text: &str) {
        self.text.insert_str(self.cursor, text);
        self.cursor += text.len();
    }

    fn backspace(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.text.replace_range(index..self.cursor, "");
            self.cursor = index;
        }
    }

    fn delete(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.text
                .replace_range(self.cursor..self.cursor + character.len_utf8(), "");
        }
    }

    fn left(&mut self) {
        if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    fn right(&mut self) {
        if let Some(character) = self.text[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn line_start(&self) -> usize {
        self.text[..self.cursor]
            .rfind('\n')
            .map_or(0, |index| index + 1)
    }

    fn line_end(&self) -> usize {
        self.text[self.cursor..]
            .find('\n')
            .map_or(self.text.len(), |index| self.cursor + index)
    }

    pub fn on_first_line(&self) -> bool {
        !self.text[..self.cursor].contains('\n')
    }

    pub fn on_last_line(&self) -> bool {
        !self.text[self.cursor..].contains('\n')
    }

    fn vertical(&mut self, up: bool) {
        let column = self.text[self.line_start()..self.cursor].chars().count();
        let target_start = if up {
            let start = self.line_start();
            self.text[..start.saturating_sub(1)]
                .rfind('\n')
                .map_or(0, |index| index + 1)
        } else {
            self.line_end() + 1
        };
        let line = &self.text[target_start..];
        let line = line.split('\n').next().unwrap_or("");
        let offset: usize = line.chars().take(column).map(char::len_utf8).sum();
        self.cursor = target_start + offset;
    }

    fn set(&mut self, text: &str) {
        self.text = text.to_owned();
        self.cursor = self.text.len();
    }

    pub fn lines(&self) -> usize {
        self.text.split('\n').count()
    }
}

/// An approval or question waiting for the owner.
#[derive(Debug, Clone)]
pub struct OpenRequest {
    pub request: OwnerRequest,
    saved: Editor,
}

/// Everything the TUI shows.
#[derive(Debug)]
pub struct UiState {
    pub input: Editor,
    sent: Vec<String>,
    recall: Option<usize>,
    pub status: Status,
    /// A turn or command holds the session.
    pub busy: bool,
    pub turn_started: Option<Instant>,
    queued: Vec<MessageId>,
    pub jobs: usize,
    pub request: Option<OpenRequest>,
    last_ctrl_c: Option<Instant>,
    quit_warned: Option<Instant>,
    quit_after_turn: bool,
    stopping: bool,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

/// The message text and how it should be handled.
fn classify(text: &str) -> Action {
    let trimmed = text.trim();
    match trimmed {
        "/compact" => Action::Compact,
        "/quit" => Action::Quit,
        "/help" => Action::Print(HELP.lines().map(PrintLine::dim).collect()),
        "/job" => Action::Print(vec![PrintLine::dim(format!("  · {JOB_USAGE}"))]),
        _ => match trimmed
            .strip_prefix("/job ")
            .or_else(|| trimmed.strip_prefix("/job\n"))
        {
            Some(command) if !command.trim().is_empty() => {
                Action::StartJob(command.trim().to_owned())
            }
            _ => Action::Submit(text.to_owned()),
        },
    }
}

impl UiState {
    pub fn new() -> Self {
        Self {
            input: Editor::default(),
            sent: Vec::new(),
            recall: None,
            status: Status::Idle,
            busy: false,
            turn_started: None,
            queued: Vec::new(),
            jobs: 0,
            request: None,
            last_ctrl_c: None,
            quit_warned: None,
            quit_after_turn: false,
            stopping: false,
        }
    }

    pub fn queued(&self) -> usize {
        self.queued.len()
    }

    /// The status line's text.
    pub fn status_text(&self, now: Instant) -> String {
        let mut text = match (&self.status, self.request.is_some()) {
            (_, true) | (Status::WaitingForOwner, _) => "Waiting for you".to_owned(),
            (Status::Idle, _) => "Idle".to_owned(),
            (Status::Thinking, _) => {
                let elapsed = self
                    .turn_started
                    .map_or(0, |start| now.saturating_duration_since(start).as_secs());
                format!("Thinking {elapsed}s")
            }
            (Status::Running(title), _) => format!("Running {title}"),
            (Status::Compacting, _) => "Compacting".to_owned(),
        };
        if !self.queued.is_empty() {
            text.push_str(&format!(" · {} queued", self.queued.len()));
        }
        if self.jobs > 0 {
            text.push_str(&format!(
                " · {} job{}",
                self.jobs,
                if self.jobs == 1 { "" } else { "s" }
            ));
        }
        text
    }

    /// The key hint at the right of the status line.
    pub fn hint(&self) -> &'static str {
        if self.request.is_some() || self.busy {
            "Esc stop"
        } else {
            "/help"
        }
    }

    /// The options line shown in place of the input while a request is open.
    pub fn options_line(&self) -> Option<String> {
        let request = &self.request.as_ref()?.request;
        let mut line = request
            .options
            .iter()
            .enumerate()
            .map(|(index, option)| format!("[{}] {option}", index + 1))
            .collect::<Vec<_>>()
            .join("   ");
        if request.options.len() == 2 {
            line.push_str("    (y / n)");
        }
        Some(line)
    }

    /// Handles a key. `jobs_running` is the session's current job count.
    pub fn key(&mut self, key: KeyEvent, now: Instant) -> Vec<Action> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if self.request.is_some() {
            return self.request_key(key, ctrl);
        }
        match key.code {
            KeyCode::Esc => {
                if self.busy {
                    self.stopping = true;
                    vec![Action::Stop]
                } else {
                    Vec::new()
                }
            }
            KeyCode::Char('c') if ctrl => {
                if self.busy {
                    self.stopping = true;
                    return vec![Action::Stop];
                }
                if !self.input.text.is_empty() {
                    self.input = Editor::default();
                    self.last_ctrl_c = None;
                    return Vec::new();
                }
                if self
                    .last_ctrl_c
                    .is_some_and(|last| now.saturating_duration_since(last) <= DOUBLE_PRESS)
                {
                    self.last_ctrl_c = None;
                    return self.quit(now);
                }
                self.last_ctrl_c = Some(now);
                Vec::new()
            }
            KeyCode::Char('d') if ctrl => self.quit(now),
            KeyCode::Char('j') if ctrl => {
                self.input.insert("\n");
                Vec::new()
            }
            KeyCode::Enter if alt || key.modifiers.contains(KeyModifiers::SHIFT) => {
                self.input.insert("\n");
                Vec::new()
            }
            KeyCode::Enter => self.enter(now),
            KeyCode::Backspace => {
                self.input.backspace();
                Vec::new()
            }
            KeyCode::Delete => {
                self.input.delete();
                Vec::new()
            }
            KeyCode::Left => {
                self.input.left();
                Vec::new()
            }
            KeyCode::Right => {
                self.input.right();
                Vec::new()
            }
            KeyCode::Home => {
                self.input.cursor = self.input.line_start();
                Vec::new()
            }
            KeyCode::End => {
                self.input.cursor = self.input.line_end();
                Vec::new()
            }
            KeyCode::Up => {
                if self.input.on_first_line() {
                    self.recall_previous();
                } else {
                    self.input.vertical(true);
                }
                Vec::new()
            }
            KeyCode::Down => {
                if self.input.on_last_line() {
                    self.recall_next();
                } else {
                    self.input.vertical(false);
                }
                Vec::new()
            }
            KeyCode::Char(character) if !ctrl => {
                self.input.insert(character.encode_utf8(&mut [0; 4]));
                self.last_ctrl_c = None;
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Pasted text is one insertion, so a pasted line break does not send.
    pub fn paste(&mut self, text: &str) {
        if self.request.is_none() {
            self.input
                .insert(&text.replace("\r\n", "\n").replace('\r', "\n"));
        }
    }

    fn request_key(&mut self, key: KeyEvent, ctrl: bool) -> Vec<Action> {
        let count = self
            .request
            .as_ref()
            .map_or(0, |open| open.request.options.len());
        let chosen = match key.code {
            KeyCode::Esc => return self.answer(OwnerAnswer::Cancelled),
            KeyCode::Char('c') if ctrl => return self.answer(OwnerAnswer::Cancelled),
            KeyCode::Char('y') if count == 2 => 0,
            KeyCode::Char('n') if count == 2 => 1,
            KeyCode::Char(digit @ '1'..='9') => {
                let index = digit as usize - '1' as usize;
                if index >= count {
                    return Vec::new();
                }
                index
            }
            _ => return Vec::new(),
        };
        self.answer(OwnerAnswer::Chosen(chosen))
    }

    fn answer(&mut self, answer: OwnerAnswer) -> Vec<Action> {
        let Some(open) = self.request.take() else {
            return Vec::new();
        };
        self.input = open.saved;
        let mut actions = Vec::new();
        if let OwnerAnswer::Chosen(index) = answer {
            let option = open.request.options.get(index).cloned().unwrap_or_default();
            actions.push(Action::Print(vec![PrintLine::dim(format!(
                "  · Answered: {option}"
            ))]));
        } else {
            self.stopping = true;
        }
        actions.push(Action::Answer(answer));
        actions
    }

    fn enter(&mut self, now: Instant) -> Vec<Action> {
        if self.input.text.trim().is_empty() {
            return Vec::new();
        }
        let text = std::mem::take(&mut self.input.text);
        self.input.cursor = 0;
        self.recall = None;
        if self.sent.last() != Some(&text) {
            self.sent.push(text.clone());
        }
        match classify(&text) {
            Action::Submit(message) => {
                let mut lines: Vec<PrintLine> = Vec::new();
                for (index, line) in message.split('\n').enumerate() {
                    let prefix = if index == 0 { "› " } else { "  " };
                    lines.push(PrintLine::plain(format!("{prefix}{line}")));
                }
                if (self.busy || !self.queued.is_empty())
                    && let Some(last) = lines.last_mut()
                {
                    last.text.push_str(" (queued)");
                }
                vec![Action::Print(lines), Action::Submit(message)]
            }
            Action::Quit => self.quit(now),
            other => vec![other],
        }
    }

    fn recall_previous(&mut self) {
        if self.sent.is_empty() {
            return;
        }
        let index = match self.recall {
            Some(0) => 0,
            Some(index) => index - 1,
            None => self.sent.len() - 1,
        };
        self.recall = Some(index);
        let text = self.sent[index].clone();
        self.input.set(&text);
    }

    fn recall_next(&mut self) {
        let Some(index) = self.recall else { return };
        if index + 1 < self.sent.len() {
            self.recall = Some(index + 1);
            let text = self.sent[index + 1].clone();
            self.input.set(&text);
        } else {
            self.recall = None;
            self.input = Editor::default();
        }
    }

    /// Ctrl+D, `/quit`, or a second Ctrl+C.
    fn quit(&mut self, now: Instant) -> Vec<Action> {
        if self.busy {
            self.quit_after_turn = true;
            self.stopping = true;
            return vec![Action::Stop];
        }
        self.quit_when_idle(now)
    }

    fn quit_when_idle(&mut self, now: Instant) -> Vec<Action> {
        if self.jobs > 0
            && !self
                .quit_warned
                .is_some_and(|warned| now.saturating_duration_since(warned) <= QUIT_AGAIN)
        {
            self.quit_warned = Some(now);
            return vec![Action::Print(vec![PrintLine::warning(format!(
                "! {} job{} running. Quit again to stop {}.",
                self.jobs,
                if self.jobs == 1 { "" } else { "s" },
                if self.jobs == 1 { "it" } else { "them" }
            ))])];
        }
        vec![Action::Quit]
    }

    /// Opens an approval or question and prints it in full.
    pub fn open_request(&mut self, request: OwnerRequest) -> Vec<Action> {
        let mut lines = vec![PrintLine::plain(format!("  {}", request.title))];
        for line in request
            .body
            .split('\n')
            .filter(|_| !request.body.is_empty())
        {
            lines.push(PrintLine::plain(format!("  {line}")));
        }
        let saved = std::mem::take(&mut self.input);
        self.request = Some(OpenRequest { request, saved });
        vec![Action::Print(lines)]
    }

    /// The request's waiter went away: its time ran out, or Stop ended it.
    pub fn request_closed(&mut self) -> Vec<Action> {
        let Some(open) = self.request.take() else {
            return Vec::new();
        };
        self.input = open.saved;
        if self.stopping {
            Vec::new()
        } else {
            vec![Action::Print(vec![PrintLine::dim(
                "  · Not answered in time",
            )])]
        }
    }

    /// Handles one session event.
    pub fn event(&mut self, event: SessionEvent, now: Instant) -> Vec<Action> {
        match event {
            SessionEvent::Queued(id) => {
                self.queued.push(id);
                Vec::new()
            }
            SessionEvent::JobMessageQueued { id, job_id } => {
                self.queued.push(id);
                vec![Action::Print(vec![PrintLine::dim(format!(
                    "  · Job {job_id} finished"
                ))])]
            }
            SessionEvent::TurnStarted(id) => {
                self.queued.retain(|queued| *queued != id);
                self.busy = true;
                self.stopping = false;
                self.turn_started = Some(now);
                Vec::new()
            }
            SessionEvent::Status(status) => {
                if status == Status::Compacting {
                    self.busy = true;
                }
                self.status = status;
                Vec::new()
            }
            SessionEvent::ToolStarted { title, .. } => {
                vec![Action::Print(vec![PrintLine::dim(format!("  · {title}"))])]
            }
            SessionEvent::ToolFinished { .. } => Vec::new(),
            SessionEvent::Notice(text) => {
                vec![Action::Print(vec![PrintLine::dim(format!("  · {text}"))])]
            }
            SessionEvent::AssistantText(text) | SessionEvent::Reply { text, .. } => {
                vec![Action::Print(
                    text.split('\n').map(PrintLine::plain).collect(),
                )]
            }
            SessionEvent::TurnEnded { outcome, .. } => {
                self.busy = false;
                self.turn_started = None;
                let mut actions = match outcome {
                    TurnOutcome::Completed => Vec::new(),
                    TurnOutcome::Stopped => {
                        vec![Action::Print(vec![PrintLine::dim("  · Stopped")])]
                    }
                    TurnOutcome::Failed(reason) => {
                        vec![Action::Print(vec![PrintLine::warning(format!(
                            "! Turn failed: {reason}"
                        ))])]
                    }
                };
                if self.quit_after_turn {
                    self.quit_after_turn = false;
                    actions.extend(self.quit_when_idle(now));
                }
                actions
            }
            SessionEvent::Discarded(id) => {
                self.queued.retain(|queued| *queued != id);
                Vec::new()
            }
            SessionEvent::CommandStatus(text) => {
                vec![Action::Print(vec![PrintLine::dim(format!("  · {text}"))])]
            }
            SessionEvent::Jobs(count) => {
                self.jobs = count;
                Vec::new()
            }
            SessionEvent::JobFinished { .. } => Vec::new(),
        }
    }

    /// Clears the busy mark when a status says the session went idle.
    pub fn settle(&mut self) {
        if self.status == Status::Idle && self.turn_started.is_none() {
            self.busy = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    #[test]
    fn a_resume_shows_the_last_message_and_reply() {
        use crate::agent_state::{AssistantEntry, AssistantPart, UserEntry};
        let user = |id: u64, origin: MessageOrigin, text: &str| {
            HistoryEntry::User(UserEntry {
                message_id: MessageId(id),
                origin,
                text: format!("[clock]\n{text}"),
            })
        };
        let reply = |text: &str| {
            HistoryEntry::Assistant(AssistantEntry {
                parts: vec![AssistantPart::Text(text.into())],
            })
        };
        let history = vec![
            user(1, MessageOrigin::Owner, "first"),
            reply("one"),
            user(2, MessageOrigin::Owner, "second\nline"),
            reply("two"),
            user(3, MessageOrigin::Job, "job done"),
        ];
        let lines: Vec<String> = last_exchange(&history)
            .into_iter()
            .map(|line| line.text)
            .collect();
        assert_eq!(lines, ["› second", "  line", "", "two"]);
        assert!(last_exchange(&[]).is_empty());
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn ctrl(character: char) -> KeyEvent {
        KeyEvent {
            modifiers: KeyModifiers::CONTROL,
            ..key(KeyCode::Char(character))
        }
    }

    fn typed(state: &mut UiState, text: &str, now: Instant) -> Vec<Action> {
        for character in text.chars() {
            state.key(key(KeyCode::Char(character)), now);
        }
        state.key(key(KeyCode::Enter), now)
    }

    fn printed(actions: &[Action]) -> Vec<String> {
        actions
            .iter()
            .filter_map(|action| match action {
                Action::Print(lines) => Some(
                    lines
                        .iter()
                        .map(|line| line.text.clone())
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn messages_during_a_turn_are_marked_queued() {
        let now = Instant::now();
        let mut state = UiState::new();
        state.event(SessionEvent::Queued(MessageId(1)), now);
        state.event(SessionEvent::TurnStarted(MessageId(1)), now);
        let actions = typed(&mut state, "second", now);
        assert_eq!(printed(&actions), ["› second (queued)"]);
        state.event(SessionEvent::Queued(MessageId(2)), now);
        typed(&mut state, "third", now);
        state.event(SessionEvent::Queued(MessageId(3)), now);
        assert!(state.status_text(now).ends_with("2 queued"));
        state.event(SessionEvent::TurnStarted(MessageId(2)), now);
        assert!(state.status_text(now).ends_with("1 queued"));
    }

    #[test]
    fn tool_calls_and_notices_print_in_order_before_the_reply() {
        let now = Instant::now();
        let mut state = UiState::new();
        let mut lines = Vec::new();
        for event in [
            SessionEvent::ToolStarted {
                call_id: "a".into(),
                tool: "read_file".into(),
                title: "read_file a.txt".into(),
            },
            SessionEvent::ToolStarted {
                call_id: "b".into(),
                tool: "memory".into(),
                title: "memory write editor".into(),
            },
            SessionEvent::Notice("Remembered: editor".into()),
            SessionEvent::Reply {
                turn_id: MessageId(1),
                text: "Done.".into(),
            },
        ] {
            lines.extend(printed(&state.event(event, now)));
        }
        assert_eq!(
            lines,
            [
                "  · read_file a.txt",
                "  · memory write editor",
                "  · Remembered: editor",
                "Done."
            ]
        );
    }

    #[test]
    fn esc_and_ctrl_c_stop_a_turn() {
        let now = Instant::now();
        for stop in [key(KeyCode::Esc), ctrl('c')] {
            let mut state = UiState::new();
            state.event(SessionEvent::TurnStarted(MessageId(1)), now);
            assert_eq!(state.key(stop, now), [Action::Stop]);
            let actions = state.event(
                SessionEvent::TurnEnded {
                    turn_id: MessageId(1),
                    outcome: TurnOutcome::Stopped,
                },
                now,
            );
            assert_eq!(printed(&actions), ["  · Stopped"]);
        }
    }

    #[test]
    fn ctrl_c_twice_quits_only_within_two_seconds() {
        let now = Instant::now();
        let mut state = UiState::new();
        assert!(state.key(ctrl('c'), now).is_empty());
        assert_eq!(
            state.key(ctrl('c'), now + Duration::from_secs(1)),
            [Action::Quit]
        );
        let mut slow = UiState::new();
        assert!(slow.key(ctrl('c'), now).is_empty());
        assert!(slow.key(ctrl('c'), now + Duration::from_secs(3)).is_empty());
        // With text, Ctrl+C clears it.
        let mut clearing = UiState::new();
        clearing.key(key(KeyCode::Char('x')), now);
        assert!(clearing.key(ctrl('c'), now).is_empty());
        assert_eq!(clearing.input.text, "");
    }

    fn request(options: &[&str]) -> OwnerRequest {
        OwnerRequest {
            call_id: "c".into(),
            title: "Approve? google.gmail_send_message".into(),
            body: "to: anna@example.com\nbody:\nHi Anna,\nFriday works.".into(),
            options: options.iter().map(|option| (*option).to_owned()).collect(),
        }
    }

    #[test]
    fn a_two_option_request_takes_y_n_or_esc() {
        let now = Instant::now();
        let mut state = UiState::new();
        state.key(key(KeyCode::Char('h')), now);
        let lines = printed(&state.open_request(request(&["Approve", "Decline"])));
        assert_eq!(lines[0], "  Approve? google.gmail_send_message");
        assert_eq!(lines.len(), 5, "the body is never shortened");
        assert_eq!(
            state.options_line().unwrap(),
            "[1] Approve   [2] Decline    (y / n)"
        );
        assert!(
            state.key(key(KeyCode::Char('x')), now).is_empty(),
            "other keys do nothing"
        );
        let actions = state.key(key(KeyCode::Char('y')), now);
        assert_eq!(printed(&actions), ["  · Answered: Approve"]);
        assert!(actions.contains(&Action::Answer(OwnerAnswer::Chosen(0))));
        assert_eq!(state.input.text, "h", "the earlier input returns");

        state.open_request(request(&["Approve", "Decline"]));
        assert!(
            state
                .key(key(KeyCode::Esc), now)
                .contains(&Action::Answer(OwnerAnswer::Cancelled))
        );

        state.open_request(request(&["KL1", "KL2", "KL3", "None of these"]));
        assert!(
            state
                .key(key(KeyCode::Char('3')), now)
                .contains(&Action::Answer(OwnerAnswer::Chosen(2)))
        );
    }

    #[test]
    fn an_expired_request_says_so() {
        let mut state = UiState::new();
        state.open_request(request(&["Yes", "No"]));
        assert_eq!(
            printed(&state.request_closed()),
            ["  · Not answered in time"]
        );
        assert!(state.request.is_none());
    }

    #[test]
    fn pasted_line_breaks_do_not_send() {
        let now = Instant::now();
        let mut state = UiState::new();
        state.paste("one\ntwo");
        assert_eq!(state.input.text, "one\ntwo");
        let actions = state.key(key(KeyCode::Enter), now);
        assert!(actions.contains(&Action::Submit("one\ntwo".into())));
        assert_eq!(printed(&actions), ["› one", "  two"]);
    }

    #[test]
    fn commands_are_recognized_only_as_whole_messages() {
        let now = Instant::now();
        let mut state = UiState::new();
        assert_eq!(typed(&mut state, "/compact", now), [Action::Compact]);
        assert!(
            typed(&mut state, "/compacting", now).contains(&Action::Submit("/compacting".into()))
        );
        assert_eq!(
            typed(&mut state, "/job cargo test", now),
            [Action::StartJob("cargo test".into())]
        );
        assert_eq!(
            printed(&typed(&mut state, "/job", now)),
            [format!("  · {JOB_USAGE}")]
        );
        assert_eq!(typed(&mut state, "/quit", now), [Action::Quit]);
    }

    #[test]
    fn quitting_with_jobs_warns_first() {
        let now = Instant::now();
        let mut state = UiState::new();
        state.event(SessionEvent::Jobs(1), now);
        let actions = state.key(ctrl('d'), now);
        assert_eq!(
            printed(&actions),
            ["! 1 job running. Quit again to stop it."]
        );
        assert_eq!(
            state.key(ctrl('d'), now + Duration::from_secs(2)),
            [Action::Quit]
        );
    }

    #[test]
    fn quitting_during_a_turn_stops_it_first() {
        let now = Instant::now();
        let mut state = UiState::new();
        state.event(SessionEvent::TurnStarted(MessageId(1)), now);
        assert_eq!(state.key(ctrl('d'), now), [Action::Stop]);
        let actions = state.event(
            SessionEvent::TurnEnded {
                turn_id: MessageId(1),
                outcome: TurnOutcome::Stopped,
            },
            now,
        );
        assert!(actions.contains(&Action::Quit));
    }

    #[test]
    fn up_and_down_recall_sent_messages() {
        let now = Instant::now();
        let mut state = UiState::new();
        typed(&mut state, "first", now);
        typed(&mut state, "second", now);
        state.key(key(KeyCode::Up), now);
        assert_eq!(state.input.text, "second");
        state.key(key(KeyCode::Up), now);
        assert_eq!(state.input.text, "first");
        state.key(key(KeyCode::Down), now);
        assert_eq!(state.input.text, "second");
        state.key(key(KeyCode::Down), now);
        assert_eq!(state.input.text, "");
    }
}
