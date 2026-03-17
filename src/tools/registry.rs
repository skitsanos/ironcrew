use std::collections::HashMap;

use super::Tool;
use crate::llm::provider::ToolSchema;
use crate::utils::error::Result;

pub struct ToolRegistry {
    tools: HashMap<String, Box<dyn Tool>>,
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
        self.tools.insert(tool.name().to_string(), tool);
    }

    #[allow(dead_code)]
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.get(name).map(|t| t.as_ref())
    }

    #[allow(dead_code)]
    pub fn list(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }

    #[allow(dead_code)]
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
