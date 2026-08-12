use std::collections::HashMap;
use std::sync::Arc;

use crate::engine::agent::Agent;
use crate::engine::eventbus::EventBus;
use crate::engine::memory::MemoryStore;
use crate::engine::messagebus::MessageBus;
use crate::engine::model_router::ModelRouter;
use crate::engine::run_history::{RunRecord, RunStatus};
use crate::engine::task::{Task, TaskResult};
use crate::llm::provider::LlmProvider;
use crate::tools::registry::ToolRegistry;
use crate::utils::error::Result;

const DEFAULT_MAX_AGENTS: usize = 64;
const HARD_MAX_AGENTS: usize = 1024;
const DEFAULT_MAX_TASKS: usize = 256;
const HARD_MAX_TASKS: usize = 10_000;
const DEFAULT_MAX_GOAL_BYTES: usize = 64 * 1024;
const HARD_MAX_GOAL_BYTES: usize = 1024 * 1024;
const MAX_PROVIDER_NAME_BYTES: usize = 128;
const MAX_MODEL_NAME_BYTES: usize = 1024;
const MAX_BASE_URL_BYTES: usize = 4096;
const MAX_API_KEY_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_APPROVAL_PATTERNS: usize = 128;
const HARD_MAX_APPROVAL_PATTERNS: usize = 1024;
const MAX_APPROVAL_PATTERN_BYTES: usize = 512;
const HARD_MAX_TOOL_ROUNDS: usize = 1000;

fn configured_count_limit(name: &str, default: usize, hard_max: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(raw) => {
            let value = raw.parse::<usize>().map_err(|_| {
                crate::utils::error::IronCrewError::Validation(format!(
                    "{name} must be an integer between 1 and {hard_max}"
                ))
            })?;
            if value == 0 || value > hard_max {
                return Err(crate::utils::error::IronCrewError::Validation(format!(
                    "{name} must be between 1 and {hard_max}; got {value}"
                )));
            }
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => {
            Err(crate::utils::error::IronCrewError::Validation(format!(
                "{name} must contain valid UTF-8"
            )))
        }
    }
}

// Re-export items from new submodules so existing import paths continue to work
#[allow(unused_imports)]
pub use crate::engine::collaborative::execute_collaborative_task;
#[allow(unused_imports)]
pub use crate::engine::condition::evaluate_condition;
#[allow(unused_imports)]
pub use crate::engine::executor::{
    TaskExecutionContext, execute_task_standalone, execute_task_standalone_with_hooks,
};
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
    pub model_router: ModelRouter,
    pub prompt_cache_key: Option<String>,
    pub prompt_cache_retention: Option<String>,
    pub eventbus: EventBus,
    /// Human-input transport for the agent-facing `ask_human` tool.
    /// Injected by `crew:run()` from the per-run context (with the real
    /// run id + store bound); `None` when no human is reachable.
    pub ask_human: Option<crate::engine::input_bridge::AskHumanContext>,
    /// Tool names / `prefix*` globs that need a human sign-off before
    /// executing (`require_approval` in Crew.new / config.lua). Unioned
    /// with `IRONCREW_REQUIRE_APPROVAL` at agent-tool finalization.
    pub require_approval: Vec<String>,
    /// Lua bytecode for before_task hooks, keyed by agent name.
    pub before_task_hooks: HashMap<String, Vec<u8>>,
    /// Lua bytecode for after_task hooks, keyed by agent name.
    pub after_task_hooks: HashMap<String, Vec<u8>>,
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
            model_router: ModelRouter::new(),
            prompt_cache_key: None,
            prompt_cache_retention: None,
            eventbus: EventBus::default(),
            ask_human: None,
            require_approval: Vec::new(),
            before_task_hooks: HashMap::new(),
            after_task_hooks: HashMap::new(),
        }
    }

    pub fn add_agent(&mut self, agent: Agent) -> Result<()> {
        let limit =
            configured_count_limit("IRONCREW_MAX_AGENTS", DEFAULT_MAX_AGENTS, HARD_MAX_AGENTS)?;
        if self.agents.len() >= limit {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Crew exceeds IRONCREW_MAX_AGENTS ({limit})"
            )));
        }
        if self
            .agents
            .iter()
            .any(|existing| existing.name == agent.name)
        {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Duplicate agent name: {}",
                agent.name
            )));
        }
        self.agents.push(agent);
        Ok(())
    }

    pub fn add_task(&mut self, task: Task) -> Result<()> {
        let limit =
            configured_count_limit("IRONCREW_MAX_TASKS", DEFAULT_MAX_TASKS, HARD_MAX_TASKS)?;
        if self.tasks.len() >= limit {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Crew exceeds IRONCREW_MAX_TASKS ({limit})"
            )));
        }
        if self.tasks.iter().any(|existing| existing.name == task.name) {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Duplicate task name: {}",
                task.name
            )));
        }
        self.tasks.push(task);
        Ok(())
    }

    /// Validate resource-bearing fields again at execution time. `Crew`
    /// fields are public for the Rust API, so constructor-only validation in
    /// Lua would otherwise be bypassable by embedded callers.
    pub fn validate_resource_limits(&self) -> Result<()> {
        let max_agents =
            configured_count_limit("IRONCREW_MAX_AGENTS", DEFAULT_MAX_AGENTS, HARD_MAX_AGENTS)?;
        if self.agents.len() > max_agents {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Crew has {} agents, exceeds IRONCREW_MAX_AGENTS ({max_agents})",
                self.agents.len()
            )));
        }

        let max_tasks =
            configured_count_limit("IRONCREW_MAX_TASKS", DEFAULT_MAX_TASKS, HARD_MAX_TASKS)?;
        if self.tasks.len() > max_tasks {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Crew has {} tasks, exceeds IRONCREW_MAX_TASKS ({max_tasks})",
                self.tasks.len()
            )));
        }

        let max_goal = configured_count_limit(
            "IRONCREW_CREW_GOAL_MAX_BYTES",
            DEFAULT_MAX_GOAL_BYTES,
            HARD_MAX_GOAL_BYTES,
        )?;
        validate_nonempty_string("Crew goal", &self.goal, max_goal)?;
        validate_nonempty_string(
            "Crew provider",
            &self.provider_config.provider,
            MAX_PROVIDER_NAME_BYTES,
        )?;
        validate_nonempty_string(
            "Crew model",
            &self.provider_config.model,
            MAX_MODEL_NAME_BYTES,
        )?;

        if let Some(base_url) = self.provider_config.base_url.as_deref() {
            validate_nonempty_string("Crew base_url", base_url, MAX_BASE_URL_BYTES)?;
            let parsed = reqwest::Url::parse(base_url).map_err(|error| {
                crate::utils::error::IronCrewError::Validation(format!(
                    "Crew base_url must be a valid HTTP(S) URL: {error}"
                ))
            })?;
            if !matches!(parsed.scheme(), "http" | "https") {
                return Err(crate::utils::error::IronCrewError::Validation(
                    "Crew base_url must use http or https".into(),
                ));
            }
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(crate::utils::error::IronCrewError::Validation(
                    "Crew base_url must not contain embedded credentials".into(),
                ));
            }
        }

        if let Some(api_key) = self.provider_config.api_key.as_deref() {
            validate_nonempty_string("Crew api_key", api_key, MAX_API_KEY_BYTES)?;
            if api_key.trim() != api_key || api_key.chars().any(char::is_control) {
                return Err(crate::utils::error::IronCrewError::Validation(
                    "Crew api_key must not contain whitespace padding or control characters".into(),
                ));
            }
        }

        let max_patterns = configured_count_limit(
            "IRONCREW_MAX_APPROVAL_PATTERNS",
            DEFAULT_MAX_APPROVAL_PATTERNS,
            HARD_MAX_APPROVAL_PATTERNS,
        )?;
        if self.require_approval.len() > max_patterns {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Crew require_approval has {} entries, exceeds IRONCREW_MAX_APPROVAL_PATTERNS ({max_patterns})",
                self.require_approval.len()
            )));
        }
        for pattern in &self.require_approval {
            validate_nonempty_string(
                "Crew require_approval pattern",
                pattern,
                MAX_APPROVAL_PATTERN_BYTES,
            )?;
        }

        if self.max_tool_rounds == 0 || self.max_tool_rounds > HARD_MAX_TOOL_ROUNDS {
            return Err(crate::utils::error::IronCrewError::Validation(format!(
                "Crew max_tool_rounds must be between 1 and {HARD_MAX_TOOL_ROUNDS}"
            )));
        }

        Ok(())
    }

    /// Create a RunRecord from execution results.
    /// If `run_id` is provided, it is used; otherwise a new UUID is generated.
    pub fn create_run_record(
        &self,
        run_id: Option<String>,
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

        let total_tokens = results
            .iter()
            .filter_map(|r| r.token_usage.as_ref())
            .fold(0u32, |total, usage| {
                total.saturating_add(usage.total_tokens)
            });
        let cached_tokens = results
            .iter()
            .filter_map(|r| r.token_usage.as_ref())
            .fold(0u32, |total, usage| {
                total.saturating_add(usage.cached_tokens)
            });

        RunRecord {
            run_id: run_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            flow_name: self.goal.clone(),
            // In-memory assembly only. The persisted flow slug is written by
            // `save_run_intent`; `update_run_completion` never overwrites it,
            // so leaving this empty here does not affect stored scoping.
            flow: String::new(),
            status,
            started_at: started_at.to_string(),
            finished_at: finished_at.to_string(),
            duration_ms,
            task_results: results.to_vec(),
            agent_count: self.agents.len(),
            task_count: self.tasks.len(),
            total_tokens,
            cached_tokens,
            tags: Vec::new(), // set by caller (CLI --tag or API input.tags)
            // Ownership is assigned by the StateStore when the run intent is
            // persisted. This in-memory completion record is never a lease
            // authority.
            owner_instance_id: String::new(),
            lease_expires_at: String::new(),
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

fn validate_nonempty_string(label: &str, value: &str, max_bytes: usize) -> Result<()> {
    if value.trim().is_empty() {
        return Err(crate::utils::error::IronCrewError::Validation(format!(
            "{label} must not be empty"
        )));
    }
    if value.len() > max_bytes {
        return Err(crate::utils::error::IronCrewError::Validation(format!(
            "{label} is {} bytes, exceeds {max_bytes}",
            value.len()
        )));
    }
    Ok(())
}
