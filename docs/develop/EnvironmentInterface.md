# Implement the Environment Interface

## Development constraints

- Implement Alyosha in Rust and target NixOS for runtime behavior.
- Keep the agent general-purpose. The model selects task-specific actions through the available tools.

## Session setup

Create `ToolPermissions` for the workspace selected at session startup. Load the workspace's dev shell, if it has one. [Project dev shell](#project-dev-shell) Store `ToolPermissions` in the session's `ToolContext` with the working directory, access to the private session log store, and the sandbox configuration, including the dev shell's environment. Pass the context to the Tool Executor. For each admitted call the executor gives the handler a context limited to that caller's scope. [Environment design](../design/EnvironmentInterface.md) · [Tool Executor](ToolExecutor.md)

## Handler interface

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

`open_file` returns an authorized read handle. `ReadWorkspace` limits the open to workspace files so `edit_file` can read an existing target. `replace_file` commits the complete replacement to an authorized workspace target. `run_shell` enforces isolation and the host timeout, then returns the command status and captured output. A denied file operation or a sandbox setup failure returns a `ToolError` to the handler and executor. A command that ran and exited with a nonzero status returns that status and its output. [File and shell enforcement](../reference/ToolPermissions.md) · [Built-in tool contracts](../reference/BuiltInTools.md)

## Atomic file replacement

Resolve and authorize the destination using the workspace rules, including symlink resolution. Stage the complete new contents in a temporary file created exclusively in the authorized target's parent directory. Preserve the ordinary permission bits of any existing file and sync the temporary file before committing. [Workspace file access](../reference/ToolPermissions.md#workspace-file-access) · [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)

For `edit_file`, snapshot the resolved file's identity and original contents when reading it through `ReadWorkspace`. Validate the exact match before staging. Recheck the snapshot just before commit and return a tool error if the target has changed or disappeared. This check catches changes observed before the rename.

Commit by renaming the temporary file into place relative to the authorized parent directory. Enforce workspace containment for the commit as well. On a failure before commit, remove the temporary file and return a `ToolError`. Treat the rename as the commit point and return success after it completes. V1 promises atomic visibility at the destination. A power-loss durability guarantee would also require syncing the parent directory. [Linux `rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html) · [Linux `fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)

## Bubblewrap launch contract

Launch a fresh sandbox for each `run_command` using `bwrap` from `pkgs.bubblewrap`. Build its argument list from trusted session and package configuration. Invoke the packaged Bash executable with `--noprofile --norc -c` and pass the model's command as a single argument. Require unprivileged user namespaces and return a tool error if the sandbox cannot start. [Bubblewrap manual](https://github.com/containers/bubblewrap/blob/v0.12.0/bwrap.xml)

| Path inside the sandbox | Contents |
| --- | --- |
| `/workspace` | The selected workspace, bound writable. Set the shell's working directory to the session's corresponding directory under this mount. |
| `/skills` | The skills directory, bound read-only with `--ro-bind` when it exists. [Skills paths](Skills.md#skills-paths) |
| `/nix/store` | The whole store, bound read-only. It holds no secrets by design, and the container already sees all of it. This makes every dev shell's tools available without computing closures. Never bind the Nix daemon socket or `/nix/var`. |
| `/tmp` and `/home/alyosha` | Fresh scratch storage and a fresh home directory for this invocation. |
| `/home/alyosha/.cache` | The persistent shell cache, bound writable. [Shell cache](#shell-cache) |
| `/dev` and `/proc` | Bubblewrap's minimal device tree and procfs for the sandbox's PID namespace. |

Keep host service sockets, Alyosha's provider credentials, and private session logs outside the exposed mounts. Resolve `/workspace/...` to the selected workspace in `open_file` so paths printed by the shell also work in file tools. With `Read` access on a direct call, resolve `/skills/...` to the skills directory, read-only. Enforce the caller's scope and workspace containment when opening the target. [Tool permissions](../reference/ToolPermissions.md)

Use `--unshare-user`, `--unshare-pid`, and `--unshare-ipc`. Run Bubblewrap inside a fresh network namespace from `pasta`, as below. Build the environment from nothing in this order: the dev shell's exported variables, if any; then `PATH` with the default tool directories appended after the dev shell's entries; then the cache variables, except those the dev shell already set; then `HOME=/home/alyosha` and `TMPDIR=/tmp`, which always win. Start the launcher, and so everything under it, with niceness 10, so builds and tests leave the host responsive. Background jobs use the same launch. Use `--new-session`, attach stdin to `/dev/null`, and drain stdout and stderr concurrently on the host. Keep the Bubblewrap status pipe separate from command output. [Bubblewrap manual](https://github.com/containers/bubblewrap/blob/v0.12.0/bwrap.xml)

### Network

Each command gets outgoing internet, so package managers such as cargo, npm, and pip can download dependencies. It never reaches the container's own loopback, where GPROXY listens. Start Bubblewrap under [`pasta`](https://passt.top/passt/about/) from `pkgs.passt`, which gives the command's process tree its own network namespace with a user-mode connection to the outside, as rootless Podman does. Configure it with no inbound port forwarding and without mapping the gateway address to the container's loopback (`--no-map-gw`). Check the exact flags against the packaged `passt` release, and record them here. A failure to start `pasta` is a sandbox setup failure and returns a tool error.

The network adds downloading and running code from the internet, still confined by Bubblewrap and the container. It adds little new risk of information leaving, because the browser already has full internet access. [Browser trust](../design/Browser.md#trust)

## Project dev shell

At session start, if the workspace root holds `flake.nix`, run `nix --extra-experimental-features "nix-command flakes" print-dev-env --json <workspace>` outside the sandbox, as the session's user. It reaches the Nix daemon and its substituters the way any program in the container does. Show **Loading dev shell…** in the status area while it runs, with a 10-minute limit. Keep the result for the session.

Take the variables whose type is `exported`, except `HOME`, `PWD`, `SHELL`, `TMPDIR`, `TMP`, `TEMP`, `TEMPDIR`, and `NIX_BUILD_TOP`, and pass them to every command. The `shellHook` does not run. In a Git repository Nix sees only tracked files, so an untracked `flake.nix` is not found.

If evaluation fails or times out, show a startup warning naming the error and use the default tools. Without `flake.nix`, use the default tools: Bash, coreutils, findutils, Git, and ripgrep from Alyosha's package. To change the default list, edit the package and rebuild. [Packages from the Nix store](../design/Deployment.md#packages-from-the-nix-store)

## Shell cache

Bind `${XDG_DATA_HOME:-$HOME/.local/share}/alyosha/shell-cache` writable at `/home/alyosha/.cache`, creating it with mode `0700` on first use. Set `XDG_CACHE_HOME=/home/alyosha/.cache`, `CARGO_HOME=/home/alyosha/.cache/cargo`, and `npm_config_cache=/home/alyosha/.cache/npm`. pip and most other tools follow `XDG_CACHE_HOME`. Downloads and build caches then survive between commands and sessions.

Every workspace and session shares the one cache. It is separate from your own `~/.cargo` and other caches on the host. Alyosha does not limit or clean it. Delete the folder to reclaim space.

### Deadline and result

Start a deadline before launching Bubblewrap, covering setup and execution. It is the call's `timeout_seconds`, from 1 to 1800, or 120 seconds when the model omits it. Read the completion record from `--json-status-fd`. If Bubblewrap exits before the deadline with an `exit-code` record, return `ShellStatus(code)` and the captured output, including for nonzero codes. Bubblewrap encodes signal termination as `128 + signal`. If the launcher fails to start or exits without a command completion record, return a `ToolError` with the available diagnostic. [Bubblewrap status reporting](https://github.com/containers/bubblewrap/blob/v0.12.0/bubblewrap.c#L403-L449)

On timeout, terminate the whole sandbox, including any subprocesses, reap the launcher, and finish collecting the output captured so far. Use Bubblewrap's PID namespace and `--die-with-parent` to tie subprocess cleanup to the launcher's lifetime. Return `TimedOut` with the captured output. Stop does the same, with `Stopped` in place of `TimedOut`: `run_shell` watches the call's cancellation token alongside the deadline. The Agent Loop records that outcome and cancels unstarted calls. [Bubblewrap process cleanup](https://github.com/containers/bubblewrap/blob/v0.12.0/bwrap.xml#L562-L590) · [Agent Loop tool contract](AgentLoop.md#v1-tool-contract)

## Session logs

Give each session a 64 MiB budget for log content: the full text of shortened command output, of shortened MCP results, and of background jobs' output, which share it. [Job output](Jobs.md#job-manager) Count retained logs and in-progress capture against the same budget. Reserve space before writing capture bytes. Keep the bounded preview available independently of the temporary log. [Result limits](ContextManager.md#v1-tool-result-limits)

Publish `log_path` only when the recorded preview omits text and the complete captured output fits in the budget. If the preview includes all the output, discard the temporary capture and release its space. If writing would exceed the budget or saving fails, discard the incomplete log and keep draining command output. Preserve the actual command status and bounded preview. An MCP result follows the same rules, except that its text is complete before the preview is sized, so save it only when the preview omits text. [MCP calls](MCP.md#handle-one-call) When output was omitted, keep the reason the full log was unavailable in the result metadata.

Keep issued `log_path` references readable through direct `read_file` calls until the session ends. Open them through the session's log store for bounded reads. Remove the session's logs at session end. [Tool permissions](../reference/ToolPermissions.md)

## Verify on the host

Create a temporary workspace and log store and pass their `ToolContext` to the handlers through the Tool Executor. Exercise file reads, rejected paths, atomic replacement, and access to a log from each caller scope. These checks run locally without a model account or GPROXY. [Permission test cases](../reference/ToolPermissions.md#verification)

Run shell integration checks on NixOS with Bubblewrap, `pasta`, unprivileged user namespaces, and the Nix store. Have a command print a workspace file path and read that path through the file handler. Check a nonzero command exit, a sandbox setup error, and a timeout with a subprocess. For shortened output, follow the returned log reference through a direct read and verify that the reference expires at session end. A fake shell result can exercise the Agent Loop’s result handling.

- In a workspace with a flake dev shell that provides `cargo`, run `cargo --version` and `git --version`. Check that both work. Break the flake and start a new session. Check for the startup warning and that `cargo` is missing while `git` works.
- Run `cargo fetch` in a small Rust project. Check that it downloads, that a second command finds the crates in `/home/alyosha/.cache/cargo`, and that a new session still finds them.
- Fetch an HTTPS address from a command. Check that it works. Connect to GPROXY's port on `127.0.0.1` and on the gateway address. Check that both fail.
- Check that `/nix/var/nix/daemon-socket` is absent and that `nix shell nixpkgs#hello` fails.
- Run `nice` in a command. Check that it prints `10`. Verifying isolation and process cleanup requires starting the actual sandbox. [Bubblewrap launch contract](#bubblewrap-launch-contract) · [Shell result contract](../reference/BuiltInTools.md#run_command)

[Architecture overview](../README.md)
