use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::utils::error::{IronCrewError, Result};

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
}


#[derive(Debug, Clone, Serialize)]
pub struct TaskResult {
    pub task: String,
    pub agent: String,
    pub output: String,
    pub success: bool,
    pub duration_ms: u64,
}

/// Validate dependency references and detect cycles.
pub fn validate_dependency_graph(tasks: &[Task]) -> Result<()> {
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
            adjacency.entry(dep.as_str()).or_default().push(task.name.as_str());
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

/// Group tasks into execution phases for parallel execution.
/// Tasks in the same phase have no dependencies on each other and can run concurrently.
pub fn topological_phases(tasks: &[Task]) -> Vec<Vec<&Task>> {
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
#[allow(dead_code)]
pub fn topological_sort(tasks: &[Task]) -> Vec<&Task> {
    let task_map: HashMap<&str, &Task> = tasks.iter().map(|t| (t.name.as_str(), t)).collect();
    let mut in_degree: HashMap<&str, usize> = HashMap::new();
    let mut adjacency: HashMap<&str, Vec<&str>> = HashMap::new();

    for task in tasks {
        in_degree.entry(task.name.as_str()).or_insert(0);
        adjacency.entry(task.name.as_str()).or_default();
        for dep in &task.depends_on {
            adjacency.entry(dep.as_str()).or_default().push(task.name.as_str());
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
