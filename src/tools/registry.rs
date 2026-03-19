use std::collections::HashMap;
use std::sync::Arc;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::Result;

#[derive(Clone)]
pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), Arc::from(tool));
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    pub fn list(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    #[allow(dead_code)] // used in integration tests
    pub fn schemas(&self) -> Vec<ToolSchema> {
        self.tools.values().map(|t| t.schema()).collect()
    }

    pub fn schemas_for(&self, tool_names: &[String]) -> Vec<ToolSchema> {
        tool_names
            .iter()
            .filter_map(|name| self.tools.get(name).map(|t| t.schema()))
            .collect()
    }

    pub async fn execute(&self, name: &str, args: serde_json::Value) -> Result<String> {
        let tool = self.tools.get(name).ok_or_else(|| {
            crate::utils::error::IronCrewError::ToolExecution {
                tool: name.to_string(),
                message: "Tool not found".into(),
            }
        })?;
        tool.execute(args).await
    }
}
