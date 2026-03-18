use std::sync::Arc;

use crate::engine::agent::Agent;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;
use crate::engine::run_history::{RunRecord, RunStatus};
use crate::engine::task::{Task, TaskResult};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::Result;

// Re-export items from new submodules so existing import paths continue to work
#[allow(unused_imports)]
pub use crate::engine::collaborative::execute_collaborative_task;
#[allow(unused_imports)]
pub use crate::engine::condition::evaluate_condition;
#[allow(unused_imports)]
pub use crate::engine::executor::{execute_task_standalone, TaskExecutionContext};
#[allow(unused_imports)]
pub use crate::engine::foreach::execute_foreach_task;
#[allow(unused_imports)]
pub use crate::engine::orchestrator::run_crew;

// used from Lua
#[allow(dead_code)]
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
    pub max_concurrent_tasks: Option<usize>,
    pub memory: MemoryStore,
    pub messagebus: MessageBus,
    pub stream: bool,
}

impl Crew {
    pub fn new(goal: String, provider_config: ProviderConfig, memory: MemoryStore) -> Self {
        Self {
            goal,
            agents: Vec::new(),
            tasks: Vec::new(),
            provider_config,
            max_tool_rounds: 10,
            max_concurrent_tasks: None,
            memory,
            messagebus: MessageBus::new(),
            stream: false,
        }
    }

    pub fn add_agent(&mut self, agent: Agent) {
        self.agents.push(agent);
    }

    pub fn add_task(&mut self, task: Task) {
        self.tasks.push(task);
    }

    /// Create a RunRecord from execution results.
    pub fn create_run_record(
        &self,
        results: &[TaskResult],
        started_at: &str,
        finished_at: &str,
        duration_ms: u64,
    ) -> RunRecord {
        let all_success = results.iter().all(|r| r.success);
        let any_success = results.iter().any(|r| r.success);
        let status = if all_success {
            RunStatus::Success
        } else if any_success {
            RunStatus::PartialFailure
        } else {
            RunStatus::Failed
        };

        RunRecord {
            run_id: uuid::Uuid::new_v4().to_string(),
            flow_name: self.goal.clone(),
            status,
            started_at: started_at.to_string(),
            finished_at: finished_at.to_string(),
            duration_ms,
            task_results: results.to_vec(),
            agent_count: self.agents.len(),
            task_count: self.tasks.len(),
        }
    }

    pub async fn run(
        &self,
        provider: Arc<dyn LlmProvider>,
        tool_registry: &ToolRegistry,
    ) -> Result<Vec<TaskResult>> {
        crate::engine::orchestrator::run_crew(self, provider, tool_registry).await
    }
}
