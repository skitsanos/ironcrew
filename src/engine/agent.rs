use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::utils::error::IronCrewError;

/// Validate a single entry in an agent's `tools` list. Returns an error for
/// malformed `agent__<name>` entries; silently accepts any other string
/// (built-ins, MCP tools, custom Lua tools — those get their own validation
/// elsewhere).
///
/// Rules for the `agent__` prefix:
/// * Total length ≤ 64 characters (OpenAI/Anthropic function-name cap).
/// * Suffix (`agent__` stripped) must be non-empty.
/// * Suffix first character: ASCII lowercase letter (`[a-z]`).
/// * Remaining characters: ASCII alphanumeric, `_`, or `-`.
pub(crate) fn validate_agent_tool_name(name: &str) -> Result<(), IronCrewError> {
    if !name.starts_with("agent__") {
        return Ok(());
    }
    if name.len() > 64 {
        return Err(IronCrewError::Validation(format!(
            "Agent tool name '{}' exceeds 64 characters (OpenAI/Anthropic function-name cap)",
            name
        )));
    }
    let suffix = &name["agent__".len()..];
    let valid = !suffix.is_empty()
        && suffix
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
        && suffix
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !valid {
        return Err(IronCrewError::Validation(format!(
            "Agent tool name '{}' is malformed: suffix must start with [a-z] and contain only [a-zA-Z0-9_-]",
            name
        )));
    }
    Ok(())
}

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
    pub fn select<'a>(agents: &'a [Agent], task: &super::task::Task) -> &'a Agent {
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

#[cfg(test)]
mod agent_validation_tests {
    use super::*;

    /// Helper: validate a list of tool name strings, returning the first error
    /// (or Ok if all pass). Mirrors what `agent_from_lua_table` does at parse
    /// time so that tests exercise the same logic path.
    fn validate_tools(tools: Vec<&str>) -> Result<(), IronCrewError> {
        for t in &tools {
            validate_agent_tool_name(t)?;
        }
        Ok(())
    }

    #[test]
    fn valid_agent_tool_names_accepted() {
        for t in [
            "agent__researcher",
            "agent__writer",
            "agent__a1b2",
            "agent__short",
            "agent__with-hyphen",
            "agent__with_underscore",
        ] {
            assert!(validate_tools(vec![t]).is_ok(), "rejected: {}", t);
        }
    }

    #[test]
    fn agent_tool_uppercase_rejected() {
        let err = validate_tools(vec!["agent__BadCase"])
            .unwrap_err()
            .to_string();
        assert!(err.contains("agent__"), "wrong error: {err}");
    }

    #[test]
    fn agent_tool_starts_with_digit_rejected() {
        assert!(validate_tools(vec!["agent__1start"]).is_err());
    }

    #[test]
    fn agent_tool_empty_suffix_rejected() {
        assert!(validate_tools(vec!["agent__"]).is_err());
    }

    #[test]
    fn agent_tool_name_too_long_rejected() {
        // "agent__" (7 chars) + 58 chars = 65, exceeds 64
        let long = format!("agent__{}", "a".repeat(58));
        assert!(validate_tools(vec![&long]).is_err());
    }

    #[test]
    fn agent_tool_exactly_64_chars_accepted() {
        // "agent__" (7 chars) + 57 chars = 64, exact limit
        let boundary = format!("agent__{}", "a".repeat(57));
        assert!(validate_tools(vec![&boundary]).is_ok());
    }

    #[test]
    fn regular_tool_names_still_accepted() {
        assert!(validate_tools(vec!["http_request", "file_read", "mcp__git__git_status"]).is_ok());
    }
}
