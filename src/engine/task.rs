use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::utils::error::{IronCrewError, Result};

const DEFAULT_MAX_TASK_RETRIES: u32 = 10;
const HARD_MAX_TASK_RETRIES: u32 = 100;
const DEFAULT_MAX_TASK_TIMEOUT_SECS: u64 = 86_400;
const HARD_MAX_TASK_TIMEOUT_SECS: u64 = 86_400;
const DEFAULT_MAX_RETRY_BACKOFF_SECS: f64 = 300.0;
const HARD_MAX_RETRY_BACKOFF_SECS: f64 = 3_600.0;
const DEFAULT_MAX_COLLABORATIVE_TURNS: usize = 100;
const HARD_MAX_COLLABORATIVE_TURNS: usize = 1_000;

fn env_limit<T>(name: &str, default: T, min: T, max: T) -> T
where
    T: std::str::FromStr + PartialOrd + Copy,
{
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value| *value >= min && *value <= max)
        .unwrap_or(default)
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Task {
    pub name: String,
    pub description: String,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub expected_output: Option<String>,
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub retry_backoff_secs: Option<f64>,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub on_error: Option<String>,
    #[serde(default)]
    pub task_type: Option<String>, // "standard" (default) or "collaborative"
    #[serde(default)]
    pub collaborative_agents: Vec<String>, // agent names for collaborative tasks
    #[serde(default)]
    pub max_turns: Option<usize>, // max conversation turns (default 3)
    #[serde(default)]
    pub foreach_source: Option<String>, // key in results to iterate over (JSON array)
    #[serde(default)]
    pub foreach_as: Option<String>, // variable name for the current item (default: "item")
    #[serde(default)]
    pub foreach_parallel: bool, // if true, process foreach items concurrently
    #[serde(default)]
    pub stream: bool, // if true, stream LLM response to stderr in real-time
    #[serde(default)]
    pub model: Option<String>, // per-task model override
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskTokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cached_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskResult {
    pub task: String,
    pub agent: String,
    pub output: String,
    pub success: bool,
    pub duration_ms: u64,
    #[serde(default)]
    pub token_usage: Option<TaskTokenUsage>,
    /// Reasoning/thinking captured from the model (Anthropic thinking blocks,
    /// OpenAI-compat reasoning_content). Persisted to run records when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
}

fn validate_task_runtime_limits(task: &Task) -> Result<()> {
    let max_retries = env_limit(
        "IRONCREW_MAX_TASK_RETRIES",
        DEFAULT_MAX_TASK_RETRIES,
        0,
        HARD_MAX_TASK_RETRIES,
    );
    if task.max_retries.is_some_and(|value| value > max_retries) {
        return Err(IronCrewError::Validation(format!(
            "Task '{}' max_retries exceeds IRONCREW_MAX_TASK_RETRIES ({})",
            task.name, max_retries
        )));
    }

    if let Some(backoff) = task.retry_backoff_secs {
        let max_backoff = env_limit(
            "IRONCREW_MAX_RETRY_BACKOFF_SECS",
            DEFAULT_MAX_RETRY_BACKOFF_SECS,
            f64::EPSILON,
            HARD_MAX_RETRY_BACKOFF_SECS,
        );
        if !backoff.is_finite() || backoff <= 0.0 || backoff > max_backoff {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' retry_backoff_secs must be finite, greater than 0, and at most {} seconds",
                task.name, max_backoff
            )));
        }
    }

    if let Some(timeout) = task.timeout_secs {
        let max_timeout = env_limit(
            "IRONCREW_MAX_TASK_TIMEOUT_SECS",
            DEFAULT_MAX_TASK_TIMEOUT_SECS,
            1,
            HARD_MAX_TASK_TIMEOUT_SECS,
        );
        if timeout == 0 || timeout > max_timeout {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' timeout_secs must be between 1 and {}",
                task.name, max_timeout
            )));
        }
    }

    if let Some(max_turns) = task.max_turns {
        let turn_limit = env_limit(
            "IRONCREW_MAX_COLLABORATIVE_TURNS",
            DEFAULT_MAX_COLLABORATIVE_TURNS,
            1,
            HARD_MAX_COLLABORATIVE_TURNS,
        );
        if max_turns == 0 || max_turns > turn_limit {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' max_turns must be between 1 and {}",
                task.name, turn_limit
            )));
        }
    }

    Ok(())
}

/// Validate dependency references and detect cycles.
pub fn validate_dependency_graph(tasks: &[Task]) -> Result<()> {
    validate_unique_task_names(tasks)?;

    for task in tasks {
        validate_task_runtime_limits(task)?;
    }

    let task_names: HashSet<&str> = tasks.iter().map(|t| t.name.as_str()).collect();

    // Check all depends_on references resolve
    for task in tasks {
        for dep in &task.depends_on {
            if !task_names.contains(dep.as_str()) {
                return Err(IronCrewError::Validation(format!(
                    "Task '{}' depends on '{}', which does not exist",
                    task.name, dep
                )));
            }
        }
    }

    // Detect cycles using Kahn's algorithm
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in tasks {
        in_degree.entry(task.name.as_str()).or_insert(0);
        adjacency.entry(task.name.as_str()).or_default();
        for dep in &task.depends_on {
            adjacency
                .entry(dep.as_str())
                .or_default()
                .push(task.name.as_str());
            *in_degree.entry(task.name.as_str()).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&name, _)| name)
        .collect();

    let mut visited = 0;

    while let Some(node) = queue.pop_front() {
        visited += 1;
        if let Some(neighbors) = adjacency.get(node) {
            for &neighbor in neighbors {
                let deg = in_degree.get_mut(neighbor).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(neighbor);
                }
            }
        }
    }

    if visited != tasks.len() {
        // Find the cycle for error message
        let in_cycle: Vec<&str> = in_degree
            .iter()
            .filter(|&(_, &deg)| deg > 0)
            .map(|(&name, _)| name)
            .collect();
        return Err(IronCrewError::Validation(format!(
            "Circular dependency detected involving tasks: {}",
            in_cycle.join(", ")
        )));
    }

    Ok(())
}

fn validate_unique_task_names(tasks: &[Task]) -> Result<()> {
    let mut names = HashSet::new();
    for task in tasks {
        if !names.insert(task.name.as_str()) {
            return Err(IronCrewError::Validation(format!(
                "Duplicate task name: {}",
                task.name
            )));
        }
    }
    Ok(())
}

/// Group tasks into execution phases for parallel execution.
/// Tasks in the same phase have no dependencies on each other and can run concurrently.
pub fn topological_phases(tasks: &[Task]) -> Vec<Vec<&Task>> {
    if let Err(err) = validate_unique_task_names(tasks) {
        tracing::error!("{}", err);
        return Vec::new();
    }

    let task_map: HashMap<&str, &Task> = tasks.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in tasks {
        in_degree.entry(task.name.as_str()).or_insert(0);
        adjacency.entry(task.name.as_str()).or_default();
        for dep in &task.depends_on {
            adjacency
                .entry(dep.as_str())
                .or_default()
                .push(task.name.as_str());
            *in_degree.entry(task.name.as_str()).or_insert(0) += 1;
        }
    }

    let mut phases = Vec::new();

    loop {
        // Collect all nodes with in_degree 0
        let ready: Vec<&str> = in_degree
            .iter()
            .filter(|&(_, &deg)| deg == 0)
            .map(|(&name, _)| name)
            .collect();

        if ready.is_empty() {
            break;
        }

        // Build this phase
        let phase: Vec<&Task> = ready
            .iter()
            .filter_map(|name| task_map.get(name).copied())
            .collect();

        // Remove these nodes and update in-degrees
        for &name in &ready {
            if let Some(neighbors) = adjacency.get(name) {
                for &neighbor in neighbors {
                    if let Some(deg) = in_degree.get_mut(neighbor) {
                        *deg -= 1;
                    }
                }
            }
            in_degree.remove(name);
        }

        phases.push(phase);
    }

    phases
}

/// Topologically sort tasks. Assumes validate_dependency_graph passed.
#[allow(dead_code)] // used in integration tests
pub fn topological_sort(tasks: &[Task]) -> Vec<&Task> {
    if let Err(err) = validate_unique_task_names(tasks) {
        tracing::error!("{}", err);
        return Vec::new();
    }

    let task_map: HashMap<&str, &Task> = tasks.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in tasks {
        in_degree.entry(task.name.as_str()).or_insert(0);
        adjacency.entry(task.name.as_str()).or_default();
        for dep in &task.depends_on {
            adjacency
                .entry(dep.as_str())
                .or_default()
                .push(task.name.as_str());
            *in_degree.entry(task.name.as_str()).or_insert(0) += 1;
        }
    }

    let mut queue: VecDeque<&str> = in_degree
        .iter()
        .filter(|&(_, &deg)| deg == 0)
        .map(|(&name, _)| name)
        .collect();

    let mut sorted = Vec::new();

    while let Some(node) = queue.pop_front() {
        sorted.push(*task_map.get(node).unwrap());
        if let Some(neighbors) = adjacency.get(node) {
            for &neighbor in neighbors {
                let deg = in_degree.get_mut(neighbor).unwrap();
                *deg -= 1;
                if *deg == 0 {
                    queue.push_back(neighbor);
                }
            }
        }
    }

    sorted
}

#[cfg(test)]
mod runtime_limit_tests {
    use super::*;

    fn task(name: &str) -> Task {
        Task {
            name: name.to_string(),
            description: "test".to_string(),
            ..Task::default()
        }
    }

    #[test]
    fn rejects_non_finite_retry_backoff() {
        let mut candidate = task("unsafe-backoff");
        candidate.retry_backoff_secs = Some(f64::NAN);
        assert!(validate_dependency_graph(&[candidate]).is_err());
    }

    #[test]
    fn rejects_zero_timeout() {
        let mut zero_timeout = task("zero-timeout");
        zero_timeout.timeout_secs = Some(0);
        assert!(validate_dependency_graph(&[zero_timeout]).is_err());
    }
}
