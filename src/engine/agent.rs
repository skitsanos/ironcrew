use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ResponseFormat {
    Text,
    JsonObject,
    JsonSchema {
        name: String,
        schema: serde_json::Value,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Agent {
    pub name: String,
    pub goal: String,
    #[serde(default)]
    pub expected_output: Option<String>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
    #[serde(default)]
    pub temperature: Option<f32>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub response_format: Option<ResponseFormat>,
}


pub struct AgentSelector;

impl AgentSelector {
    /// Select the best agent for a task using heuristic scoring.
    /// Weights: capability_match=0.4, tool_match=0.3, goal_alignment=0.3
    pub fn select<'a>(
        agents: &'a [Agent],
        task: &super::task::Task,
    ) -> &'a Agent {
        agents
            .iter()
            .enumerate()
            .max_by(|(idx_a, a), (idx_b, b)| {
                let score_a = Self::score(a, task);
                let score_b = Self::score(b, task);
                score_a
                    .partial_cmp(&score_b)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    // On tie, prefer earlier agent (lower index = Greater so it wins)
                    .then(idx_b.cmp(idx_a))
            })
            .map(|(_, agent)| agent)
            .expect("agents list must not be empty")
    }

    pub fn score(agent: &Agent, task: &super::task::Task) -> f32 {
        let task_words: HashSet<String> = task
            .description
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|s| !s.is_empty())
            .collect();

        // Capability match (weight: 0.4)
        let cap_score = if agent.capabilities.is_empty() {
            0.0
        } else {
            let matched = agent
                .capabilities
                .iter()
                .filter(|cap| task_words.contains(&cap.to_lowercase()))
                .count();
            (matched as f32 / agent.capabilities.len() as f32).min(1.0)
        };

        // Tool match (weight: 0.3)
        let tool_keywords: Vec<&str> = vec![
            "scrape", "web", "file", "write", "read", "shell", "execute", "command",
        ];
        let referenced_tools: Vec<&str> = tool_keywords
            .iter()
            .filter(|kw| task_words.contains(**kw))
            .copied()
            .collect();

        let tool_score = if referenced_tools.is_empty() {
            1.0 // no tools referenced = all agents equal
        } else if agent.tools.is_empty() {
            0.0
        } else {
            let matched = agent
                .tools
                .iter()
                .filter(|tool| {
                    let tool_lower = tool.to_lowercase();
                    referenced_tools.iter().any(|kw| tool_lower.contains(kw))
                })
                .count();
            (matched as f32 / referenced_tools.len() as f32).min(1.0)
        };

        // Goal alignment (weight: 0.3)
        let goal_words: HashSet<String> = agent
            .goal
            .to_lowercase()
            .split_whitespace()
            .map(|s| s.trim_matches(|c: char| !c.is_alphanumeric()).to_string())
            .filter(|s| !s.is_empty())
            .collect();

        let goal_score = if task_words.is_empty() {
            0.0
        } else {
            let overlap = task_words.intersection(&goal_words).count();
            overlap as f32 / task_words.len() as f32
        };

        0.4 * cap_score + 0.3 * tool_score + 0.3 * goal_score
    }
}
