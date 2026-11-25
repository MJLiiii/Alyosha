//! The TUI: the conversation prints into the terminal's scrollback, with a
//! live status line and input box in a small inline viewport below it.
//!
//! See `docs/design/TUI.md` and `docs/develop/TUI.md`.

pub mod state;

use std::{
    io::{IsTerminal, Stdout, Write},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use alyosha_config::Paths;
use crossterm::{
    event::{
        DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyEventKind,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    terminal::{disable_raw_mode, enable_raw_mode, supports_keyboard_enhancement},
};
use futures_util::StreamExt;
use ratatui::{
    Terminal, TerminalOptions, Viewport,
    backend::CrosstermBackend,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use self::state::{Action, LineStyle, MAX_INPUT_LINES, PrintLine, UiState};
use crate::{
    BoxFuture,
    agent_loop::{JobDelivery, SessionDriver},
    approvals::{Approver, OwnerAnswer, OwnerRequest},
    session::{self, FrontEnd},
};

/// Two status lines plus the input box at its largest.
const VIEWPORT_HEIGHT: u16 = 2 + MAX_INPUT_LINES as u16;

type Ask = (OwnerRequest, oneshot::Sender<OwnerAnswer>);

/// The TUI's way of asking: the request goes to the UI task, which answers
/// through the responder.
struct TuiApprover {
    requests: mpsc::UnboundedSender<Ask>,
}

impl Approver for TuiApprover {
    fn ask(&self, request: OwnerRequest, cancel: CancellationToken) -> BoxFuture<'_, OwnerAnswer> {
        Box::pin(async move {
            let (responder, answer) = oneshot::channel();
            if self.requests.send((request, responder)).is_err() {
                return OwnerAnswer::Undeliverable;
            }
            tokio::select! {
                answer = answer => answer.unwrap_or(OwnerAnswer::Undeliverable),
                () = cancel.cancelled() => OwnerAnswer::Cancelled,
            }
        })
    }
}

/// Runs the TUI in `workspace` (the current folder by default). Returns the
/// process exit status.
pub fn run(workspace: Option<String>) -> u8 {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        eprintln!("alyosha: the TUI needs a terminal. Programs should use `alyosha acp`.");
        return 2;
    }
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => {
            eprintln!("alyosha: {error}");
            return 2;
        }
    };
    let config = match alyosha_config::load_from(&paths.config_file()) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("alyosha: {error}");
            return 2;
        }
    };
    let requested = workspace.map_or_else(
        || std::env::current_dir().unwrap_or_default(),
        PathBuf::from,
    );
    let workspace = match session::resolve_workspace(&requested) {
        Ok(workspace) => workspace,
        Err(error) => {
            eprintln!("alyosha: {error}");
            return 2;
        }
    };
    let log_file = alyosha_config::logging::init("alyosha-tui", config.timezone, &paths).ok();
    let runtime = tokio::runtime::Runtime::new().expect("a Tokio runtime starts");
    runtime.block_on(async move {
        let mut terminal = match setup_terminal() {
            Ok(terminal) => terminal,
            Err(error) => {
                eprintln!("alyosha: could not set up the terminal: {error}");
                return 1;
            }
        };
        let code = session_loop(&mut terminal, &config, &paths, &workspace, log_file).await;
        restore_terminal();
        let _ = terminal.show_cursor();
        println!();
        code
    })
}

fn setup_terminal() -> std::io::Result<Terminal<CrosstermBackend<Stdout>>> {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    if supports_keyboard_enhancement().unwrap_or(false) {
        crossterm::execute!(
            stdout,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
    }
    crossterm::execute!(stdout, EnableBracketedPaste)?;
    Terminal::with_options(
        CrosstermBackend::new(stdout),
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )
}

fn restore_terminal() {
    let mut stdout = std::io::stdout();
    let _ = crossterm::execute!(stdout, DisableBracketedPaste);
    if supports_keyboard_enhancement().unwrap_or(false) {
        let _ = crossterm::execute!(stdout, PopKeyboardEnhancementFlags);
    }
    let _ = disable_raw_mode();
    let _ = stdout.flush();
}

fn home_relative(path: &std::path::Path, home: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

async fn session_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    config: &alyosha_config::Config,
    paths: &Paths,
    workspace: &std::path::Path,
    log_file: Option<PathBuf>,
) -> u8 {
    let mut ui = UiState::new();
    print(
        terminal,
        &[PrintLine {
            text: format!(
                "alyosha · {} · {}",
                home_relative(workspace, &paths.home),
                config.model.name
            ),
            style: LineStyle::Plain,
        }],
    );
    let (requests, mut asks) = mpsc::unbounded_channel::<Ask>();
    let starting = if workspace.join("flake.nix").is_file() {
        "Loading dev shell…"
    } else {
        "Starting…"
    };
    let _ = terminal.draw(|frame| render_static(frame, starting));
    let started = session::start(
        config,
        paths,
        workspace,
        session::model_client(config, paths),
        FrontEnd {
            approver: Some(Arc::new(TuiApprover { requests })),
            job_delivery: JobDelivery::Queue,
            channel_line: None,
            log_file,
            on_loading_dev_shell: None,
        },
    )
    .await;
    let mut started = match started {
        Ok(started) => started,
        Err(error) => {
            print(
                terminal,
                &[PrintLine {
                    text: format!("! {error}"),
                    style: LineStyle::Warning,
                }],
            );
            return 1;
        }
    };
    let warnings: Vec<PrintLine> = started
        .warnings
        .iter()
        .map(|warning| PrintLine {
            text: format!("! {warning}"),
            style: LineStyle::Warning,
        })
        .collect();
    print(terminal, &warnings);
    let driver = started.driver.clone();
    let mut keys = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut responder: Option<oneshot::Sender<OwnerAnswer>> = None;
    loop {
        draw(terminal, &ui);
        let actions = tokio::select! {
            event = keys.next() => match event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => ui.key(key, Instant::now()),
                Some(Ok(Event::Paste(text))) => { ui.paste(&text); Vec::new() }
                Some(Ok(Event::Resize(_, _))) => { let _ = terminal.autoresize(); Vec::new() }
                Some(Ok(_)) => Vec::new(),
                Some(Err(_)) | None => vec![Action::Quit],
            },
            event = started.events.recv() => match event {
                Some(event) => { let actions = ui.event(event, Instant::now()); ui.settle(); actions }
                None => vec![Action::Quit],
            },
            ask = asks.recv() => match ask {
                Some((request, answer)) => { responder = Some(answer); ui.open_request(request) }
                None => Vec::new(),
            },
            _ = tick.tick() => {
                if responder.as_ref().is_some_and(oneshot::Sender::is_closed) {
                    responder = None;
                    ui.request_closed()
                } else {
                    Vec::new()
                }
            }
        };
        for action in actions {
            match action {
                Action::Print(lines) => print(terminal, &lines),
                Action::Submit(text) => {
                    driver.submit(text);
                }
                Action::Stop => driver.stop(),
                Action::Compact => driver.compact(),
                Action::StartJob(command) => {
                    driver.start_job(&command);
                }
                Action::Answer(answer) => {
                    if let Some(responder) = responder.take() {
                        let _ = responder.send(answer);
                    }
                    if answer == OwnerAnswer::Cancelled {
                        driver.stop();
                    }
                }
                Action::Quit => {
                    end(terminal, &driver).await;
                    return 0;
                }
            }
        }
    }
}

async fn end(terminal: &mut Terminal<CrosstermBackend<Stdout>>, driver: &SessionDriver) {
    let _ = terminal.draw(|frame| render_static(frame, "Ending the session…"));
    driver.end().await;
    let _ = terminal.clear();
}

/// Wraps text to the terminal width at print time.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(10);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        if raw.width() <= width {
            lines.push(raw.to_owned());
            continue;
        }
        let mut line = String::new();
        let mut line_width = 0;
        for word in raw.split_inclusive(' ') {
            let word_width = word.width();
            if line_width + word_width > width && !line.is_empty() {
                lines.push(line.trim_end().to_owned());
                line.clear();
                line_width = 0;
            }
            if word_width > width {
                for character in word.chars() {
                    let character_width = character.width().unwrap_or(0);
                    if line_width + character_width > width {
                        lines.push(std::mem::take(&mut line));
                        line_width = 0;
                    }
                    line.push(character);
                    line_width += character_width;
                }
            } else {
                line.push_str(word);
                line_width += word_width;
            }
        }
        lines.push(line.trim_end().to_owned());
    }
    lines
}

fn style(line_style: LineStyle) -> Style {
    match line_style {
        LineStyle::Plain => Style::default(),
        LineStyle::Dim => Style::default().add_modifier(Modifier::DIM),
        LineStyle::Warning => Style::default().fg(Color::Yellow),
    }
}

fn print(terminal: &mut Terminal<CrosstermBackend<Stdout>>, lines: &[PrintLine]) {
    if lines.is_empty() {
        return;
    }
    let width = terminal
        .size()
        .map(|size| size.width as usize)
        .unwrap_or(80);
    let mut rendered: Vec<Line> = Vec::new();
    for line in lines {
        for piece in wrap(&line.text, width) {
            rendered.push(Line::styled(piece, style(line.style)));
        }
    }
    let height = rendered.len() as u16;
    let _ = terminal.insert_before(height, |buffer| {
        let area = buffer.area;
        ratatui::widgets::Widget::render(Paragraph::new(rendered), area, buffer);
    });
}

fn render_static(frame: &mut ratatui::Frame, status: &str) {
    let area = frame.area();
    let separator = "─".repeat(area.width as usize);
    frame.render_widget(
        Paragraph::new(vec![
            Line::styled(separator, Style::default().add_modifier(Modifier::DIM)),
            Line::from(format!(" {status}")),
        ]),
        area,
    );
}

fn draw(terminal: &mut Terminal<CrosstermBackend<Stdout>>, ui: &UiState) {
    let _ = terminal.draw(|frame| {
        let area = frame.area();
        let width = area.width as usize;
        let status = format!(" {}", ui.status_text(Instant::now()));
        let hint = format!("{} ", ui.hint());
        let gap = width.saturating_sub(status.width() + hint.width());
        let mut lines = vec![
            Line::styled(
                "─".repeat(width),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Line::from(vec![
                Span::raw(status),
                Span::raw(" ".repeat(gap)),
                Span::styled(hint, Style::default().add_modifier(Modifier::DIM)),
            ]),
        ];
        let mut cursor = None;
        if let Some(options) = ui.options_line() {
            lines.push(Line::from(format!(" {options}")));
        } else {
            let text = &ui.input.text;
            let before = &text[..ui.input.cursor];
            let cursor_line = before.matches('\n').count();
            let cursor_column = before.rsplit('\n').next().unwrap_or("").width();
            let total = text.split('\n').count();
            let first = (cursor_line + 1)
                .saturating_sub(MAX_INPUT_LINES)
                .min(total.saturating_sub(1));
            for (index, line) in text
                .split('\n')
                .enumerate()
                .skip(first)
                .take(MAX_INPUT_LINES)
            {
                let prefix = if index == 0 { "› " } else { "  " };
                lines.push(Line::from(format!("{prefix}{line}")));
            }
            let row = 2 + cursor_line - first;
            cursor = Some((area.x + 2 + cursor_column as u16, area.y + row as u16));
        }
        frame.render_widget(Paragraph::new(lines), area);
        if let Some((x, y)) = cursor {
            frame.set_cursor_position((x.min(area.right().saturating_sub(1)), y));
        }
    });
}

#[cfg(test)]
mod tests {
    use ratatui::backend::TestBackend;

    use super::*;

    #[test]
    fn long_lines_wrap_at_the_width() {
        assert_eq!(wrap("one two three four", 10), ["one two", "three four"]);
        assert_eq!(wrap("short\nlines", 10), ["short", "lines"]);
        assert_eq!(
            wrap(&"x".repeat(25), 10),
            ["xxxxxxxxxx", "xxxxxxxxxx", "xxxxx"]
        );
    }

    #[test]
    fn the_viewport_shows_status_and_options() {
        let mut terminal = Terminal::new(TestBackend::new(50, 4)).unwrap();
        let mut ui = UiState::new();
        ui.open_request(OwnerRequest {
            call_id: "c".into(),
            title: "Approve? x.y".into(),
            body: String::new(),
            options: vec!["Approve".into(), "Decline".into()],
        });
        terminal
            .draw(|frame| {
                let area = frame.area();
                let status = format!(" {}", ui.status_text(Instant::now()));
                frame.render_widget(
                    Paragraph::new(vec![
                        Line::from(status),
                        Line::from(format!(" {}", ui.options_line().unwrap())),
                    ]),
                    area,
                );
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let text: String = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol().to_owned())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        assert!(text.contains("Waiting for you"));
        assert!(text.contains("[1] Approve   [2] Decline    (y / n)"));
    }
}
