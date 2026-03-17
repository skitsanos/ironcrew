use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::engine::agent::{Agent, AgentSelector};
use crate::engine::task::{validate_dependency_graph, topological_sort, Task, TaskResult};
use crate::llm::provider::*;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::{IronCrewError, Result};

pub struct ProviderConfig {
    pub provider: String,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key: Option<String>,
}

pub struct Crew {
    pub goal: String,
    pub agents: Vec<Agent>,
    pub tasks: Vec<Task>,
    pub provider_config: ProviderConfig,
    pub max_tool_rounds: usize,
}

impl Crew {
    pub fn new(goal: String, provider_config: ProviderConfig) -> Self {
        Self {
            goal,
            agents: Vec::new(),
            tasks: Vec::new(),
            provider_config,
            max_tool_rounds: 10,
        }
    }

    pub fn add_agent(&mut self, agent: Agent) {
        self.agents.push(agent);
    }

    pub fn add_task(&mut self, task: Task) {
        self.tasks.push(task);
    }

    pub async fn run(
        &self,
        provider: &dyn LlmProvider,
        tool_registry: &ToolRegistry,
    ) -> Result<Vec<TaskResult>> {
        if self.agents.is_empty() {
            return Err(IronCrewError::Validation("No agents in crew".into()));
        }
        if self.tasks.is_empty() {
            return Err(IronCrewError::Validation("No tasks in crew".into()));
        }

        validate_dependency_graph(&self.tasks)?;
        let sorted_tasks = topological_sort(&self.tasks);

        let mut results: HashMap<String, TaskResult> = HashMap::new();
        let mut failed_tasks: HashSet<String> = HashSet::new();

        for task in &sorted_tasks {
            // Check if any dependency failed
            let dep_failed = task.depends_on.iter().find(|dep| failed_tasks.contains(*dep));
            if let Some(failed_dep) = dep_failed {
                let result = TaskResult {
                    task: task.name.clone(),
                    agent: String::new(),
                    output: format!("Skipped: dependency '{}' failed", failed_dep),
                    success: false,
                    duration_ms: 0,
                };
                failed_tasks.insert(task.name.clone());
                results.insert(task.name.clone(), result);
                tracing::warn!("Skipping task '{}': dependency '{}' failed", task.name, failed_dep);
                continue;
            }

            // Select agent
            let agent = if let Some(ref agent_name) = task.agent {
                self.agents
                    .iter()
                    .find(|a| a.name == *agent_name)
                    .ok_or_else(|| IronCrewError::Validation(format!(
                        "Task '{}' assigned to unknown agent '{}'",
                        task.name, agent_name
                    )))?
            } else {
                AgentSelector::select(&self.agents, task)
            };

            tracing::info!("Task '{}' assigned to agent '{}'", task.name, agent.name);

            let start = Instant::now();

            match self
                .execute_task(task, agent, provider, tool_registry, &results)
                .await
            {
                Ok(output) => {
                    let result = TaskResult {
                        task: task.name.clone(),
                        agent: agent.name.clone(),
                        output,
                        success: true,
                        duration_ms: start.elapsed().as_millis() as u64,
                    };
                    tracing::info!(
                        "Task '{}' completed in {}ms",
                        task.name,
                        result.duration_ms
                    );
                    results.insert(task.name.clone(), result);
                }
                Err(e) => {
                    let result = TaskResult {
                        task: task.name.clone(),
                        agent: agent.name.clone(),
                        output: e.to_string(),
                        success: false,
                        duration_ms: start.elapsed().as_millis() as u64,
                    };
                    tracing::error!("Task '{}' failed: {}", task.name, e);
                    failed_tasks.insert(task.name.clone());
                    results.insert(task.name.clone(), result);
                }
            }
        }

        // Return results in sorted order
        Ok(sorted_tasks
            .iter()
            .filter_map(|t| results.remove(&t.name))
            .collect())
    }

    async fn execute_task(
        &self,
        task: &Task,
        agent: &Agent,
        provider: &dyn LlmProvider,
        tool_registry: &ToolRegistry,
        completed_results: &HashMap<String, TaskResult>,
    ) -> Result<String> {
        let mut messages = Vec::new();

        // System prompt
        let system_content = agent
            .system_prompt
            .clone()
            .unwrap_or_else(|| format!("You are {}. Your goal: {}", agent.name, agent.goal));
        messages.push(ChatMessage::system(&system_content));

        // Build user prompt with context
        let mut prompt_parts = vec![format!("Task: {}", task.description)];

        if let Some(ref expected) = task.expected_output {
            prompt_parts.push(format!("Expected output: {}", expected));
        }

        if let Some(ref context) = task.context {
            prompt_parts.push(format!("Additional context: {}", context));
        }

        // Inject dependency results
        for dep_name in &task.depends_on {
            if let Some(dep_result) = completed_results.get(dep_name) {
                if dep_result.success {
                    prompt_parts.push(format!(
                        "Result from '{}': {}",
                        dep_name, dep_result.output
                    ));
                }
            }
        }

        messages.push(ChatMessage::user(&prompt_parts.join("\n\n")));

        let model = agent
            .model
            .clone()
            .unwrap_or_else(|| self.provider_config.model.clone());

        // Get tool schemas for this agent
        let tool_schemas = tool_registry.schemas_for(&agent.tools);
        let has_tools = !tool_schemas.is_empty();

        let mut rounds = 0;

        loop {
            let request = ChatRequest {
                messages: messages.clone(),
                model: model.clone(),
                temperature: agent.temperature,
                max_tokens: agent.max_tokens,
                response_format: agent.response_format.clone(),
            };

            let response = if has_tools {
                provider.chat_with_tools(request, &tool_schemas).await?
            } else {
                provider.chat(request).await?
            };

            // If no tool calls, return the content
            if response.tool_calls.is_empty() {
                return response
                    .content
                    .ok_or_else(|| IronCrewError::Provider("Empty response from LLM".into()));
            }

            rounds += 1;
            if rounds > self.max_tool_rounds {
                return Err(IronCrewError::Task {
                    task: task.name.clone(),
                    message: format!(
                        "Exceeded max tool rounds ({})",
                        self.max_tool_rounds
                    ),
                });
            }

            // Add assistant message with tool calls (must include the tool_calls array)
            messages.push(ChatMessage::assistant(
                response.content.clone(),
                Some(response.tool_calls.clone()),
            ));

            // Execute tool calls and add tool result messages
            for tool_call in &response.tool_calls {
                tracing::info!("Executing tool '{}' for task '{}'", tool_call.function.name, task.name);

                let args: serde_json::Value = serde_json::from_str(&tool_call.function.arguments)
                    .unwrap_or(serde_json::Value::Object(serde_json::Map::new()));

                let tool_result = tool_registry
                    .execute(&tool_call.function.name, args)
                    .await;

                let result_text = match tool_result {
                    Ok(output) => output,
                    Err(e) => format!("Tool error: {}", e),
                };

                messages.push(ChatMessage::tool(&tool_call.id, &result_text));
            }
        }
    }
}
