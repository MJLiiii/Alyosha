# Environment Interface

The Environment Interface is how a tool handler opens a file or runs a command on the user's machine. The user selects a workspace at session startup. Alyosha uses that choice to establish the working directory, file permissions, and shell sandbox configuration for that session. These are properties of the running Alyosha session. Its file and shell behavior can be tested in a temporary workspace by invoking the handlers directly.

## From a tool call to a file

Consider a request to read `notes.txt`. The Tool Executor finds the registered `read_file` handler and checks that the caller may use it and that its arguments match the schema. The handler then asks the Environment Interface to open `notes.txt` for reading. The environment resolves it from the session working directory and checks the actual target when opening it. A symlink leading outside the workspace produces a tool error. [Tool Executor](ToolExecutor.md) · [Tool permissions](../reference/ToolPermissions.md)

The same handler serves direct calls and Code Mode scripts. On a direct call it can also read a session log: the saved full text of a shortened command output or MCP result from this session. Inside a script it receives a narrower context that permits reads of regular workspace files only. [Built-in tools](../reference/BuiltInTools.md) · [Read-only Code Mode](ToolExecutor.md#read-only-code-mode)

## File updates

`write_file` and `edit_file` stage the complete new contents in the target's directory, then replace the file atomically. The destination changes when the replacement commits. A failure before that point returns a tool error and leaves the original file in place. A new file appears only once its contents are complete. [Atomic file replacement](../develop/EnvironmentInterface.md#atomic-file-replacement)

## Shell commands

Each command runs in a fresh Bubblewrap sandbox and starts in the session's working directory under `/workspace`. The workspace is writable and the Nix store is read-only. The command's tools come from the project's Nix dev shell when the workspace has a `flake.nix`, and otherwise from a small default set. It runs at a lower CPU priority than the rest of the machine. It gets private scratch space, a persistent cache for downloads such as crates and npm packages, and internet access that cannot reach Alyosha's own services in the container, such as GPROXY. The Environment Interface enforces the command deadline and captures output for the handler. If a preview is shortened, Alyosha can keep the full log within a 64 MiB budget per session. If the log is unavailable, the result still carries the command's status and preview. [Shell and log behavior](../reference/BuiltInTools.md#run_command) · [Bubblewrap launch contract](../develop/EnvironmentInterface.md#bubblewrap-launch-contract)

In the v1 contracts, `ToolContext` carries the session's environment and `ToolPermissions` provides the operations handlers call. Model traffic to the GPROXY service belongs to the LLM Client / Gateway. MCP server processes belong to the MCP Client and run outside the shell sandbox. Skill files are visible read-only at `/skills` to direct reads and the shell. Alyosha itself runs inside a NixOS container, so the selected workspace must be under a mounted folder. The sandbox works the same there. [Deployment](Deployment.md) · [MCP Client](MCP.md#where-servers-run) · [Skills](Skills.md#bundled-files) [Environment implementation guide](../develop/EnvironmentInterface.md) · [LLM Client / Gateway](LLMClientGateway.md) · [Architecture overview](../README.md)
