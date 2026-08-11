use std::collections::HashMap;

use async_trait::async_trait;

use super::super::ToolRegistry;
use crate::llm::provider::ToolSchema;
use crate::tools::{Tool, ToolCallContext};
use crate::utils::error::Result;

pub(super) struct DefinedTool {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) dependencies: Vec<String>,
}

#[async_trait]
impl Tool for DefinedTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: self.name.clone(),
            description: self.description.clone(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn conversation_dependencies(&self) -> Vec<String> {
        self.dependencies.clone()
    }

    async fn execute(&self, _args: serde_json::Value, _ctx: &ToolCallContext) -> Result<String> {
        Ok("ok".into())
    }
}

pub(super) fn registry(description: &str) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(DefinedTool {
        name: "lookup".into(),
        description: description.into(),
        dependencies: Vec::new(),
    }));
    registry
}

fn policy_registry(tool: Box<dyn Tool>) -> ToolRegistry {
    let mut registry = ToolRegistry {
        tools: HashMap::new(),
        execution_policy: crate::tools::runtime_policy::RuntimeExecutionPolicy::from_values(60, 5),
        approval: None,
    };
    registry.register(tool);
    registry
}

pub(super) fn assert_tool_policy_drift(left: Box<dyn Tool>, right: Box<dyn Tool>, name: &str) {
    let selected = vec![name.to_string()];
    let left = policy_registry(left)
        .conversation_execution_fingerprint(&selected)
        .expect("left tool policy must fingerprint");
    let right = policy_registry(right)
        .conversation_execution_fingerprint(&selected)
        .expect("right tool policy must fingerprint");
    assert_ne!(left, right, "{name} policy drift must fence rehydration");
}
