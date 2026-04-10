use serde::Serialize;

/// Top-level crew data for the DAG visualization.
/// Serializes to the same shape as examples/graph-prototype/assets/js/data.js
#[derive(Debug, Default, Serialize)]
pub struct GraphData {
    pub name: String,
    pub provider: String,
    pub model: String,
    pub goal: String,
    pub agents: Vec<GraphAgent>,
    pub tools: Vec<GraphTool>,
    pub tasks: Vec<GraphTask>,
    #[serde(default)]
    pub functions: Vec<()>,
    #[serde(default)]
    pub memories: Vec<()>,
    #[serde(default)]
    pub conversations: Vec<()>,
    #[serde(default)]
    pub dialogs: Vec<()>,
    #[serde(default)]
    pub messages: Vec<()>,
}

#[derive(Debug, Serialize)]
pub struct GraphAgent {
    pub name: String,
    pub goal: String,
    pub capabilities: Vec<String>,
    pub tools: Vec<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
}

#[derive(Debug, Serialize)]
pub struct GraphTool {
    pub name: String,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub source: String,
}

#[derive(Debug, Serialize)]
pub struct GraphTask {
    pub id: String,
    pub name: String,
    pub task_type: String,
    pub description: String,
    pub depends_on: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_agent: Option<String>,
    pub assignment_source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_output: Option<String>,
}
