# Tool Registry

The Tool Registry is the catalog shared by the Agent Loop and the Tool Executor. Each entry pairs the definition the model sees with the handler Alyosha will run.

| Reader | What the registry supplies | How it is used |
| --- | --- | --- |
| Agent Loop | Each tool's name, description, and JSON input schema | The Context Manager budgets these definitions into normal model requests, including a retry. Summary requests use an empty tool list. |
| Tool Executor | The definition and handler for one exact name | The executor checks the caller's scope, validates the supplied arguments against the schema, and invokes the handler. |

For example, the model can request `read_file({"path":"notes/monday.md"})` using the advertised schema. The executor finds that entry and validates the arguments. Its handler uses the Environment Interface to open the file under the caller's permissions. The registry can therefore advertise the same `path` field for every session while each handler invocation uses the selected workspace. [Tool Executor](ToolExecutor.md#shared-dispatch) · [Environment Interface](EnvironmentInterface.md)

## What gets registered

At startup, the filesystem module supplies `read_file`, `write_file`, and `edit_file`. The shell module supplies `run_command` and `job`, Code Mode supplies `execute_code`, the memory module supplies `memory`, the skills module supplies `skill`, and the approvals module supplies `ask_owner`. The MCP Client then adds each configured server's tools under names such as `mcp__github__search_issues`, marking the tools your configuration lists in `ask` as needing your approval. [Approvals](Approvals.md) The model sees all these definitions on normal requests. [MCP Client](MCP.md) Inside an `execute_code` script, the host exposes only `read_file`. [Built-in tool contracts](../reference/BuiltInTools.md) · [Read-only Code Mode](ToolExecutor.md#read-only-code-mode)

The registry rejects duplicate names and invalid schemas during registration. Once startup finishes, the catalog stays fixed while turns run. MCP tools use the same registration interface, so a server tool with a rejected name or schema is skipped with a warning. [Rust implementation guide](../develop/ToolRegistry.md) · [Architecture overview](../README.md)
