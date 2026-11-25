# Implement the TUI

The TUI is a front end for the session driver, like the ACP mode. Build a minimal version for the first real-model turn, then complete it with the contracts below. [TUI design](../design/TUI.md) · [Session driver](AgentLoop.md#session-driver) · [ACP mode](Channels.md) · [Architecture overview](../README.md)

## Libraries

Use [`ratatui`](https://docs.rs/ratatui) with an inline viewport, `Viewport::Inline`, over [`crossterm`](https://docs.rs/crossterm). Print to the scrollback with `Terminal::insert_before`, which writes lines above the viewport. Pin both crates. If standard input or output is not a terminal, exit with an error that points to `alyosha acp` for use from programs.

Enable raw mode and keyboard enhancement where the terminal supports it, so Alt+Enter is distinct from Enter. Restore the terminal on every exit, including a panic, with a panic hook that disables raw mode before printing.

## Structure

Run two tasks joined by channels: the UI task, which owns the terminal, and the session driver.

```text
UiCommand     (UI → driver)
    Submit(text) | Stop | Compact | StartJob(command) | Quit

UiEvent       (driver → UI)
    Header { workspace, model }
    Warning(text)
    Queued(message_id) | TurnStarted(message_id)
    Status(Thinking | Running(tool) | WaitingForOwner | Compacting | Idle)
    Notice(text)                      # tool starts, memory writes, compaction, job starts
    Jobs(count)                       # running background jobs, for the status line
    Request(OwnerRequest, responder)  # approvals and ask_owner questions
    Reply(text)
    TurnEnded(TurnOutcome)
    CommandStatus(text)               # /compact results and the busy message
```

Keep the UI's state in one `UiState` value updated by a pure function of the previous state and one event or key, so tests can drive it without a terminal. Render after each update and once a second while a turn runs, for the elapsed time.

The viewport is two lines plus the input's height, which grows with the message up to 8 lines and scrolls inside after that.

## Printing

| Event | Scrollback |
| --- | --- |
| `Submit` | `› ` and the message. Add ` (queued)` when a turn is active. On `TurnStarted` for a queued message, print nothing more. |
| `Notice` | Two spaces, `· `, and the text, dimmed |
| `Warning` | `! ` and the text |
| `Request` | Two spaces, then the title, then each body line, indented |
| `Reply` | The text, wrapped to the terminal width at print time |
| `TurnEnded(Stopped)` | `  · Stopped` |
| `TurnEnded(Failed(reason))` | `! Turn failed: ` and the reason |
| `CommandStatus` | `  · ` and the text |

Never shorten a request body. The terminal scrollback holds it.

## Input

Keep an editable buffer with a cursor. Enter sends it, unless it is empty. Alt+Enter and Ctrl+J insert a line break. Up and Down move through this session's sent messages when the cursor is on the first or last line. Handle pasted text as one insertion, so a pasted line break does not send.

Recognize `/compact`, `/quit`, and `/help` only as the whole trimmed message, and `/job` followed by a space and a command, or alone for its usage. [`/job` command](Jobs.md#job-command) Any other text, including other words starting with `/`, is a message.

| Key | Idle | Turn or command running | Request open |
| --- | --- | --- | --- |
| Esc | Nothing | `Stop` | Answer `Cancelled`, which stops the turn |
| Ctrl+C | Clear the input. A second press on an empty input within 2 seconds quits. | `Stop` | As Esc |
| Ctrl+D, `/quit` | Quit. With jobs running, warn first. [TUI quit](Jobs.md#tui) | `Stop`, wait for the turn to close, then quit, warning first about jobs | As Esc, then quit |
| `y`, `n`, a digit | Typed | Typed | Answer the matching option. Other keys do nothing. |

On quit, end the session normally, which shuts down MCP servers, then restore the terminal. [MCP shutdown](MCP.md#start-servers-and-register-tools)

## Approvals

The TUI's `Approver` sends `Request` with a one-shot responder and waits for the first of the answer, the configured timeout, and cancellation. On timeout it prints `  · Not answered in time` and returns `NoAnswer`. While a request is open the input box shows the options as `[1] Approve   [2] Decline`, with `(y / n)` when there are two. After an answer, print `  · Answered: <option>` and restore the input box with its earlier text. [The front-end operation](Approvals.md#the-front-end-operation)

## Startup

1. Take the workspace from the first argument, or the current directory. Resolve it to an absolute path without symlinks. It must be an existing directory under `$HOME/Projects`. Otherwise print the reason and exit with status 2 before touching the terminal.
2. Start the session as the session driver describes, then send `Header` and each startup warning. [Session startup](AgentLoop.md#session-driver)

## Verification

These run under `cargo test`. Drive `UiState` with scripted events and keys, and render with ratatui's `TestBackend` where layout matters.

- Submit two messages during a turn. Check that both print with **(queued)**, the status line counts 2, and the count falls as turns start.
- Script a turn with two tool calls and a memory write. Check for three notice lines before the reply, in order.
- Press Esc during a turn, and Ctrl+C. Check that each sends `Stop` and the turn ends with **Stopped**.
- Press Ctrl+C twice on an empty input while idle. Check that the second press quits, and that two presses 3 seconds apart do not.
- Open a two-option request. Check that `y` answers the first option, that other letters are ignored, and that Esc answers `Cancelled`. Open one with four options and answer `3`.
- Let a request time out with a 1-minute limit. Check for **Not answered in time** and `NoAnswer`.
- Paste text containing line breaks. Check that nothing is sent until Enter.
- Type `/compact` during a turn. Check for the busy status and no message in history. Type `/compacting`. Check that it is sent as a message.
- Start with a path outside `~/Projects` and with a missing path. Check for exit status 2 and a clear message, with the terminal untouched.
- Panic inside the UI task. Check that raw mode is restored.

In a real terminal, which `cargo test` cannot cover:

- Check Alt+Enter in your terminal emulator, and in `machinectl shell` inside the container.
- Resize the terminal during a turn. Check that the viewport redraws and earlier scrollback is untouched.
- Select and copy a reply from the scrollback with the mouse.

[Architecture overview](../README.md)
