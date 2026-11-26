# Tool Registry

The Tool Registry is the catalog shared by the Agent Loop and the Tool Executor. Each entry pairs the definition the model sees with the handler Alyosha will run.

| Reader | What the registry supplies | How it is used |
| --- | --- | --- |
| Agent Loop | Each tool's name, description, and JSON input schema | The Context Manager budgets these definitions into normal model requests, including a retry. Summary requests use an empty tool list. |
| Tool Executor | The definition and handler for one exact name | The executor checks the caller's scope, validates the supplied arguments against the schema, and invokes the handler. |

For example, the model can request `read_file({"path":"notes/monday.md"})` using the advertised schema. The executor finds that entry and validates the arguments. Its handler uses the Environment Interface to open the file under the caller's permissions. The registry can therefore advertise the same `path` field for every session while each handler invocation uses the selected workspace. [Tool Executor](ToolExecutor.md#shared-dispatch) · [Environment Interface](EnvironmentInterface.md)

## What gets registered

At startup, the filesystem module supplies `read_file`, `write_file`, and `edit_file`. The shell module supplies `run_command` and `job`, Code Mode supplies `execute_code`, the memory module supplies `memory`, the skills module supplies `skill`, and the approvals module supplies `ask_owner`. The MCP Client then adds each configured server's tools under names such as `mcp__github__search_issues`, marking the tools your configuration lists in `ask` as needing your approval. [Approvals](Approvals.md) The model sees all these definitions on normal requests. [MCP Client](MCP.md) Inside an `execute_code` script, the host exposes only `read_file`. [Built-in tool contracts](reference/BuiltInTools.md) · [Read-only Code Mode](ToolExecutor.md#read-only-code-mode)

The registry rejects duplicate names and invalid schemas during registration. Once startup finishes, the catalog stays fixed while turns run. MCP tools use the same registration interface, so a server tool with a rejected name or schema is skipped with a warning. [Implementation](#implementation) · [Architecture overview](README.md)

## Implementation

Build one catalog at startup. Give the Agent Loop and Tool Executor read access to the same registrations. [Architecture overview](README.md)

### V1 interface

```text
ToolDefinition {
    name: String
    description: String
    input_schema: JsonValue
    requires_approval: bool   # set from an MCP server's `ask` list; false for built-ins
}

RegisteredTool {
    definition: ToolDefinition
    handler: ToolHandler
}

ToolHandler: async (args: JsonValue, context: ToolContext)   # owned; the context is cheap to clone
    -> Result<ToolOutput, ToolError>

ToolRegistry
    register(&mut self, tool: RegisteredTool) -> Result<(), RegistrationError>
    definitions(&self) -> iterator of &ToolDefinition
    get(&self, name: &str) -> Option<&RegisteredTool>
```

- `register` checks that the name is unique and its input schema is valid before inserting the entry. A failed registration leaves the catalog intact.
- `definitions` exposes the name, description, and schema from each registered entry. The Agent Loop passes these to the Context Manager for the normal request. [Context Manager](ContextManager.md#implementation)
- `get` takes the exact name supplied in a tool call. Its caller owns the per-call argument check and dispatch. [Tool Executor](ToolExecutor.md#implementation)

### Register built-in tools at startup

Each tool module supplies a constructor returning a definition paired with its async handler. Use the built-in contracts to populate each object schema's properties, types, and required fields. For example, `read_file` requires a string `path`, with optional integer `offset` (minimum 0) and `max_bytes` (minimum 1). Checking that the offset falls on a UTF-8 character boundary requires the file and belongs in the handler. Build the catalog before constructing the executor or starting the session driver. Propagate registration errors. [Built-in tool contracts](reference/BuiltInTools.md#v1-inputs-and-outputs) · [Code Mode handler](ToolExecutor.md#run-read-only-code-mode)

```rust
fn build_tool_registry(
    mcp: &McpManager,
    warnings: &mut Vec<McpWarning>,
) -> Result<ToolRegistry, RegistrationError> {
    let mut registry = ToolRegistry::new();
    registry.register(filesystem::read_file_tool())?;
    registry.register(filesystem::write_file_tool())?;
    registry.register(filesystem::edit_file_tool())?;
    registry.register(shell::run_command_tool())?;
    registry.register(shell::job_tool())?;
    registry.register(code_mode::execute_code_tool())?;
    registry.register(memory::memory_tool())?;
    registry.register(skills::skill_tool())?;
    registry.register(approvals::ask_owner_tool())?;
    for tool in mcp.tools() {
        if let Err(error) = registry.register(tool) {
            warnings.push(McpWarning::from(error)); // skip this tool, keep going
        }
    }
    Ok(registry)
}
```

The function takes the started `McpManager` and a warning list. A built-in registration error stops startup. An MCP registration error skips that tool. [MCP registration](MCP.md#start-servers-and-register-tools)

### Verification

Register a fake tool. Check that `definitions()` yields its advertised schema and that `get(name)` retrieves the paired handler. Try registering a different handler under the same name and check that the original entry survives. Submit an invalid schema for a new name and check that the entry is absent.
