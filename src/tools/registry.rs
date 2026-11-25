//! Tool Registry: the catalog shared by the Agent Loop and the Tool Executor.
//!
//! Each entry pairs the definition the model sees with the handler Alyosha
//! runs. The catalog is built at startup and stays fixed while turns run.
//! Interface: `docs/develop/ToolRegistry.md`.

use std::{collections::HashMap, fmt};

use serde_json::Value;

use super::{ToolDefinition, ToolHandler};

/// A definition paired with its handler.
#[derive(Clone)]
pub struct RegisteredTool {
    pub definition: ToolDefinition,
    pub handler: ToolHandler,
}

impl fmt::Debug for RegisteredTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RegisteredTool")
            .field("definition", &self.definition)
            .finish_non_exhaustive()
    }
}

/// Why a tool was not registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    #[error("a tool named {0} is already registered")]
    DuplicateName(String),
    #[error("tool {name} has an invalid input schema: {reason}")]
    InvalidSchema { name: String, reason: String },
}

/// The tools the model can request.
#[derive(Debug, Default)]
pub struct ToolRegistry {
    tools: Vec<RegisteredTool>,
    by_name: HashMap<String, usize>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds a tool if its name is unique and its input schema is valid. A
    /// failed registration leaves the catalog unchanged.
    pub fn register(&mut self, tool: RegisteredTool) -> Result<(), RegistrationError> {
        let name = &tool.definition.name;
        if self.by_name.contains_key(name) {
            return Err(RegistrationError::DuplicateName(name.clone()));
        }
        check_input_schema(&tool.definition.input_schema).map_err(|reason| {
            RegistrationError::InvalidSchema {
                name: name.clone(),
                reason,
            }
        })?;
        self.by_name.insert(name.clone(), self.tools.len());
        self.tools.push(tool);
        Ok(())
    }

    /// Every tool's definition, in registration order.
    pub fn definitions(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.iter().map(|tool| &tool.definition)
    }

    /// The tool with exactly this name.
    pub fn get(&self, name: &str) -> Option<&RegisteredTool> {
        self.by_name.get(name).map(|&index| &self.tools[index])
    }
}

/// A tool's input is a JSON object, so its schema must be a valid JSON Schema
/// whose root has `"type": "object"`, as model APIs and MCP require.
fn check_input_schema(schema: &Value) -> Result<(), String> {
    if schema.get("type") != Some(&Value::from("object")) {
        return Err("the root must have \"type\": \"object\"".to_owned());
    }
    jsonschema::validator_for(schema)
        .map(drop)
        .map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::tools::{ToolContext, ToolOutput, handler};

    fn tool(name: &str, schema: Value, reply: &'static str) -> RegisteredTool {
        RegisteredTool {
            definition: ToolDefinition {
                name: name.to_owned(),
                description: format!("The {name} tool."),
                input_schema: schema,
                requires_approval: false,
            },
            handler: handler(move |_args: Value, _context: ToolContext| async move {
                Ok(ToolOutput::text(reply))
            }),
        }
    }

    fn path_schema() -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        })
    }

    async fn run(tool: &RegisteredTool) -> String {
        let context = ToolContext {
            scope: crate::tools::Scope::Direct,
            cancel: Default::default(),
            session: Default::default(),
            executor: crate::tools::ToolExecutor::new(Default::default(), Default::default()),
            call_id: None,
        };
        (tool.handler)(json!({}), context).await.unwrap().text
    }

    #[tokio::test]
    async fn registered_tool_is_advertised_and_found() {
        let mut registry = ToolRegistry::new();
        registry
            .register(tool("fake", path_schema(), "original"))
            .unwrap();

        let definitions: Vec<_> = registry.definitions().collect();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].name, "fake");
        assert_eq!(definitions[0].input_schema, path_schema());

        let found = registry.get("fake").expect("the tool is registered");
        assert_eq!(run(found).await, "original");
        assert!(registry.get("Fake").is_none(), "lookup uses the exact name");
    }

    #[tokio::test]
    async fn duplicate_name_keeps_the_original() {
        let mut registry = ToolRegistry::new();
        registry
            .register(tool("fake", path_schema(), "original"))
            .unwrap();

        let error = registry
            .register(tool("fake", json!({"type": "object"}), "replacement"))
            .unwrap_err();
        assert_eq!(error, RegistrationError::DuplicateName("fake".into()));

        assert_eq!(registry.definitions().count(), 1);
        let found = registry.get("fake").unwrap();
        assert_eq!(found.definition.input_schema, path_schema());
        assert_eq!(run(found).await, "original");
    }

    #[test]
    fn invalid_schemas_are_rejected() {
        let mut registry = ToolRegistry::new();
        for schema in [
            json!({"type": "object", "properties": {"path": {"type": 5}}}),
            json!({"type": "object", "required": "path"}),
            json!({"type": "string"}),
            json!(true),
            json!({"type": "object", "properties": {"p": {"$ref": "https://example.com/s.json"}}}),
        ] {
            let error = registry
                .register(tool("bad", schema.clone(), ""))
                .unwrap_err();
            assert!(
                matches!(error, RegistrationError::InvalidSchema { .. }),
                "{schema} gave {error:?}"
            );
        }
        assert!(registry.get("bad").is_none());
        assert_eq!(registry.definitions().count(), 0);
    }
}
