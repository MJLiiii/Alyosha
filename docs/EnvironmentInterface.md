# Environment Interface

The Environment Interface is how a tool handler opens a file or runs a command on the user's machine. The user selects a workspace at session startup. Alyosha uses that choice to establish the working directory, file permissions, and shell sandbox configuration for that session. These are properties of the running Alyosha session. Its file and shell behavior can be tested in a temporary workspace by invoking the handlers directly.

## From a tool call to a file

Consider a request to read `notes.txt`. The Tool Executor finds the registered `read_file` handler and checks that the caller may use it and that its arguments match the schema. The handler then asks the Environment Interface to open `notes.txt` for reading. The environment resolves it from the session working directory and checks the actual target when opening it. A symlink leading outside the workspace produces a tool error. [Tool Executor](ToolExecutor.md) · [Tool permissions](reference/ToolPermissions.md)

The same handler serves direct calls and Code Mode scripts. On a direct call it can also read a session log: the saved full text of a shortened command output or MCP result from this session. Inside a script it receives a narrower context that permits reads of regular workspace files only. [Built-in tools](reference/BuiltInTools.md) · [Read-only Code Mode](ToolExecutor.md#read-only-code-mode)

## File updates

`write_file` and `edit_file` stage the complete new contents in the target's directory, then replace the file atomically. The destination changes when the replacement commits. A failure before that point returns a tool error and leaves the original file in place. A new file appears only once its contents are complete. [Atomic file replacement](#atomic-file-replacement)

## Shell commands

Each command runs in a systemd unit of its own, as the user `alyosha-run`, and starts in the session's working directory under `/workspace`. The unit sees only that session's working folder, writable, the Nix store and skills, read-only, a fresh home and `/tmp`, and a shared cache for downloads such as crates and npm packages. It sees only its own processes. The command's tools come from the project's Nix dev shell when the workspace has a `flake.nix`, and otherwise from a small default set. It runs at a lower CPU priority than the rest of the machine, with its own memory cap. It can reach the internet, but not Alyosha's own services in the container, such as GPROXY, nor the host or your local network. The Environment Interface enforces the command deadline and captures output for the handler. If a preview is shortened, Alyosha can keep the full log within a 64 MiB budget per session. If the log is unavailable, the result still carries the command's status and preview. [Shell and log behavior](reference/BuiltInTools.md#run_command) · [Command launch](#command-launch) · [Deployment](Deployment.md#command-units)

In the v1 contracts, `ToolContext` carries the session's environment and `ToolPermissions` provides the operations handlers call. Model traffic to the GPROXY service belongs to the LLM Client / Gateway. MCP server processes belong to the MCP Client and run outside the shell sandbox. Skill files are visible read-only at `/skills` to direct reads and the shell. Alyosha runs inside a NixOS container, and each session's workspace is one of its working folders. [Where things live](Deployment.md#where-things-live) [Deployment](Deployment.md) · [MCP Client](MCP.md#where-servers-run) · [Skills](Skills.md#bundled-files) [Implementation](#implementation) · [LLM Client / Gateway](LLMClientGateway.md) · [Architecture overview](README.md)

## Implementation

### Development constraints

- Implement Alyosha in Rust and target NixOS for runtime behavior.
- Keep the agent general-purpose. The model selects task-specific actions through the available tools.

### Session setup

Create `ToolPermissions` for the workspace selected at session startup. Load the workspace's dev shell, if it has one. [Project dev shell](#project-dev-shell) Store `ToolPermissions` in the session's `ToolContext` with the working directory, access to the private session log store, and the sandbox configuration, including the dev shell's environment. Pass the context to the Tool Executor. For each admitted call the executor gives the handler a context limited to that caller's scope. [Tool Executor](ToolExecutor.md#implementation)

### Handler interface

The proposed v1 interface uses `ToolPermissions` for the file and shell operations.

```text
ToolPermissions
    open_file(&self, path: &Path, access: FileAccess) -> Result<ScopedFile, ToolError>
    replace_file(&self, path: &Path, content: &[u8], mode: ReplaceMode)
        -> Result<(), ToolError>
    async run_shell(&self, command: &str, timeout: Duration, cancel: &CancellationToken)
        -> Result<CommandOutput, ToolError>

FileAccess
    Read
    ReadWorkspace

ReplaceMode
    CreateOrReplace
    ReplaceExisting(original: &FileSnapshot)
```

`open_file` returns an authorized read handle. `ReadWorkspace` limits the open to workspace files so `edit_file` can read an existing target. `replace_file` commits the complete replacement to an authorized workspace target. `run_shell` enforces isolation and the host timeout, then returns the command status and captured output. A denied file operation or a sandbox setup failure returns a `ToolError` to the handler and executor. A command that ran and exited with a nonzero status returns that status and its output. [File and shell enforcement](reference/ToolPermissions.md) · [Built-in tool contracts](reference/BuiltInTools.md)

### Atomic file replacement

Resolve and authorize the destination using the workspace rules, including symlink resolution. Stage the complete new contents in a temporary file created exclusively in the authorized target's parent directory. Preserve the ordinary permission bits of any existing file and sync the temporary file before committing. [Workspace file access](reference/ToolPermissions.md#workspace-file-access) · [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)

For `edit_file`, snapshot the resolved file's identity and original contents when reading it through `ReadWorkspace`. Validate the exact match before staging. Recheck the snapshot just before commit and return a tool error if the target has changed or disappeared. This check catches changes observed before the rename.

Commit by renaming the temporary file into place relative to the authorized parent directory. Enforce workspace containment for the commit as well. On a failure before commit, remove the temporary file and return a `ToolError`. Treat the rename as the commit point and return success after it completes. V1 promises atomic visibility at the destination. A power-loss durability guarantee would also require syncing the parent directory. [Linux `rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html) · [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)

### Command launch

Run each `run_command`, and each background job, in a fresh unit from the session's template, `alyosha-run-<folder>@<pid>-<n>.service`, where `<pid>` is this process's ID and `<n>` counts up from 1. The template fixes the user, mounts, and limits. [Command units](Deployment.md#command-units)

1. Create `/run/alyosha-run/<pid>-<n>/`, mode `0750`, group `alyosha-work`, and write `spec.json` into it: the command, the working directory under `/workspace`, and the environment. Create an empty `output` file there.
2. Start the unit with `systemctl --no-ask-password start`. The template's `Type=exec` makes it return once the command runs, or fail with systemd's message if the unit cannot start.
3. Inside the unit, `alyosha run-exec <instance>` reads `spec.json`, changes to the working directory, replaces its environment with the one given, and `exec`s the packaged Bash with `--noprofile --norc -c` and the command as one argument. systemd attaches standard input to `/dev/null` and standard output and error to `output`.
4. Read `output` as it grows, for the preview and the session log, and poll `systemctl show` for the unit's state, every 10 ms at first and every 200 ms at most. [Session logs](#session-logs)
5. The template keeps a finished unit as `active (exited)` or `failed` (`RemainAfterExit=yes`), so its result stays readable. Read `Result`, `ExecMainCode`, and `ExecMainStatus`, then stop the unit and reset a failed one so its name is free. Remove the run folder.

If systemd refuses the start, or the unit fails before `run-exec` starts the command, return a `ToolError` with systemd's message. This is the sandbox setup failure.

| Path in the unit | Contents |
| --- | --- |
| `/workspace` | The session's working folder, writable. |
| `/skills` | The skills directory, read-only. [Skills paths](Skills.md#skills-paths) |
| `/nix/store` | The whole store, read-only. It holds no secrets by design, and the container already sees all of it. This makes every dev shell's tools available without computing closures. The Nix daemon socket is hidden. |
| `/tmp`, `/home/alyosha` | Fresh, private scratch storage and home directory for this unit. |
| `/home/alyosha/.cache` | The persistent shell cache, writable. [Shell cache](#shell-cache) |
| `/proc` | A fresh procfs of the unit's own PID namespace. |

`/var/lib`, `/var/cache`, and the other runs' folders are empty, and the database socket and the backups are hidden. Resolve `/workspace/...` to the session's working folder in `open_file` so paths printed by the shell also work in file tools. With `Read` access on a direct call, resolve `/skills/...` to the skills directory, read-only. Enforce the caller's scope and workspace containment when opening the target. [Tool permissions](reference/ToolPermissions.md)

Build the environment from nothing in this order: the dev shell's exported variables, if any; then `PATH` with the default tool directories appended after the dev shell's entries; then the cache variables, except those the dev shell already set; then `SSL_CERT_FILE` and `NIX_SSL_CERT_FILE` naming the CA bundle's store path; then `HOME=/home/alyosha` and `TMPDIR=/tmp`, which always win.

The agent's files and the commands' files must stay usable by both users. The working folders belong to `alyosha-run` with the group `alyosha-work`, which holds `alyosha` too, and are set-group-ID, so new files take that group. The bridges and the command units run with `UMask=0007`, so new files are group-writable.

Outside the container, as in tests and development on the host, `ALYOSHA_COMMANDS=direct` runs each command as a plain child process in the workspace, in a process group the deadline and Stop kill, without any isolation. Never set it in the container.

#### Network

Each command gets outgoing internet, so package managers such as cargo, npm, and pip can download dependencies. Every unit is in `alyosha-run.slice`, and a rule in the container refuses that slice's loopback connections, so commands never reach GPROXY or any other service listening inside the container. The host refuses the whole container's connections to the host itself and to private addresses. [Network](Deployment.md#network)

Name resolution goes to the container's public nameservers, as for every program in it.

The network adds downloading and running code from the internet, still confined by the unit and the container. It adds little new risk of information leaving, because the browser already has full internet access. [Browser trust](Browser.md#trust)

### Project dev shell

At session start, if the workspace root holds `flake.nix`, run `nix --extra-experimental-features "nix-command flakes" print-dev-env --json <workspace>` outside the unit, as `alyosha`. It reaches the Nix daemon and its substituters the way any program in the container does. Show **Loading dev shell…** in the status area while it runs, with a 10-minute limit. Keep the result for the session.

Take the variables whose type is `exported`, except `HOME`, `PWD`, `SHELL`, `TMPDIR`, `TMP`, `TEMP`, `TEMPDIR`, and `NIX_BUILD_TOP`, and pass them to every command. The `shellHook` does not run. In a Git repository Nix sees only tracked files, so an untracked `flake.nix` is not found.

If evaluation fails or times out, show a startup warning naming the error and use the default tools. Without `flake.nix`, use the default tools: Bash, coreutils, findutils, Git, and ripgrep from Alyosha's package. To change the default list, edit the package and rebuild. [Packages from the Nix store](Deployment.md#packages-from-the-nix-store)

### Shell cache

The unit binds `/var/cache/alyosha/shell`, owned by `alyosha-run`, writable at `/home/alyosha/.cache`. Set `XDG_CACHE_HOME=/home/alyosha/.cache`, `CARGO_HOME=/home/alyosha/.cache/cargo`, and `npm_config_cache=/home/alyosha/.cache/npm`. pip and most other tools follow `XDG_CACHE_HOME`. Downloads and build caches then survive between commands and sessions.

Every workspace and session shares the one cache. It exists only inside the container. Alyosha does not limit or clean it. Delete the folder to reclaim space.

### Command results

Start a deadline before starting the unit, covering setup and execution. It is the call's `timeout_seconds`, from 1 to 1800, or 120 seconds when the model omits it. Watch the call's cancellation token alongside it.

| systemd reports | Result |
| --- | --- |
| `Result=success`, or `exit-code` with `ExecMainCode=exited` | `ShellStatus(ExecMainStatus)` and the captured output, including for nonzero codes |
| `ExecMainCode=killed` or `dumped` | `ShellStatus(128 + signal)`, as a shell reports it |
| `Result=oom-kill` | `ShellStatus(137)` and a note that the command passed its memory limit |
| The unit failed before the command started | A `ToolError` with systemd's message |

At the deadline, stop the unit. systemd kills its whole cgroup, so every subprocess ends with it, whatever it did to leave its process group. Finish reading `output`, and return `TimedOut` with the captured output. Stop does the same, with `Stopped` in place of `TimedOut`. The Agent Loop records that outcome and cancels unstarted calls. [Agent Loop tool contract](AgentLoop.md#v1-tool-contract) A background job uses the job's own deadline and token instead. [Background jobs](Jobs.md#implementation)

### Session logs

Give each session a 64 MiB budget for log content: the full text of shortened command output, of shortened MCP results, and of background jobs' output, which share it. [Job output](Jobs.md#job-manager) Count retained logs and in-progress capture against the same budget. Reserve space before writing capture bytes. Keep the bounded preview available independently of the temporary log. [Result limits](ContextManager.md#v1-tool-result-limits)

Publish `log_path` only when the recorded preview omits text and the complete captured output fits in the budget. If the preview includes all the output, discard the temporary capture and release its space. If writing would exceed the budget or saving fails, discard the incomplete log and keep draining command output. Preserve the actual command status and bounded preview. An MCP result follows the same rules, except that its text is complete before the preview is sized, so save it only when the preview omits text. [MCP calls](MCP.md#handle-one-call) When output was omitted, keep the reason the full log was unavailable in the result metadata.

Issue log paths as `/session-logs/<name>`, a prefix no workspace path uses. Keep issued `log_path` references readable through direct `read_file` calls until the session ends in this process, or until a compaction replaces the result that issued them. Open them through the session's log store for bounded reads. Keep each session's logs in its own folder, `/var/cache/alyosha/sessions/<process ID>-<random>`, mode `0700`. Remove the folder when the process ends the session. At startup, also remove the folders whose process ID no longer runs, which a crashed process left behind. A resumed session starts with an empty log store, and its earlier paths read as gone. [After each compaction](SessionStore.md#after-each-compaction) [Tool permissions](reference/ToolPermissions.md)

### Verify on the host

Create a temporary workspace and log store and pass their `ToolContext` to the handlers through the Tool Executor. Exercise file reads, rejected paths, atomic replacement, and access to a log from each caller scope. These checks run locally without a model account or GPROXY. [Permission test cases](reference/ToolPermissions.md#verification)

`tests/shell.rs` and `tests/jobs.rs` check statuses, output, deadlines, Stop, and jobs with `ALYOSHA_COMMANDS=direct` anywhere. The unit checks there run in the container, as `alyosha`, with `ALYOSHA_TEST_UNITS` naming a template and `ALYOSHA_TEST_WORKSPACE` its folder; `alyosha check` on the host runs the main ones. Run the shell integration checks in the container, where `alyosha` may start the command units. Have a command print a workspace file path and read that path through the file handler. Check a nonzero command exit, a sandbox setup error from a refused start, and a timeout with a subprocess. For shortened output, follow the returned log reference through a direct read and verify that the reference expires at session end. A fake shell result can exercise the Agent Loop’s result handling.

- In a workspace with a flake dev shell that provides `cargo`, run `cargo --version` and `git --version`. Check that both work. Break the flake and start a new session. Check for the startup warning and that `cargo` is missing while `git` works.
- Run `cargo fetch` in a small Rust project. Check that it downloads, that a second command finds the crates in `/home/alyosha/.cache/cargo`, and that a new session still finds them.
- Fetch an HTTPS address from a command. Check that it works. Connect to GPROXY's port on `127.0.0.1`, the host at `10.233.1.1`, and the router. Check that all three fail.
- Check that `/nix/var/nix/daemon-socket`, `/var/lib/alyosha-secrets`, `/run/postgresql`, and the other working folders are absent, and that `nix shell nixpkgs#hello` fails.
- Run `ls /proc` and `nice`. Check that only the unit's processes appear and that `nice` prints `10`.
- Allocate past the memory limit. Check the result and that the bridge keeps running.
- Have the agent write a file and a command edit it, and the other way round. Check that both succeed. [Command units](Deployment.md#command-units) · [Shell result contract](reference/BuiltInTools.md#run_command)

[Architecture overview](README.md)
