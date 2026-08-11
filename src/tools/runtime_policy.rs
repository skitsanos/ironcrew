//! Process-local execution settings captured once for one runtime.

use serde_json::{Value, json};

use crate::lua::json_policy::JsonLimits;
use crate::lua::limits::LuaLimits;
use crate::utils::error::{IronCrewError, Result};

use super::conversation_policy::ConversationTurnPolicy;
use super::http_request::HttpToolPolicy;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LuaEnvPolicy {
    ProcessAllowlist,
    PersistentConversationBlocked,
}

impl LuaEnvPolicy {
    fn definition(self) -> &'static str {
        match self {
            Self::ProcessAllowlist => "process_allowlist",
            Self::PersistentConversationBlocked => "persistent_conversation_blocked",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LuaVmPolicy {
    limits: LuaLimits,
    json_limits: JsonLimits,
    http: HttpToolPolicy,
    env: LuaEnvPolicy,
}

impl LuaVmPolicy {
    pub(crate) fn limits(&self) -> LuaLimits {
        self.limits
    }

    pub(crate) fn json_limits(&self) -> JsonLimits {
        self.json_limits
    }

    pub(crate) fn http(&self) -> HttpToolPolicy {
        self.http.clone()
    }

    pub(crate) fn env(&self) -> LuaEnvPolicy {
        self.env
    }

    pub(crate) fn definition(&self) -> Value {
        json!({
            "max_memory_bytes": self.limits.max_memory_bytes,
            "max_instructions": self.limits.max_instructions,
            "max_execution_seconds": self.limits.max_execution_time.as_secs(),
            "json": self.json_limits.definition(),
            "http": self.http.lua_definition(),
            "env": self.env.definition(),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(http_marker: usize, allow_private: bool) -> Self {
        Self {
            limits: LuaLimits::default(),
            json_limits: JsonLimits::default(),
            http: HttpToolPolicy::from_values(http_marker, allow_private),
            env: LuaEnvPolicy::ProcessAllowlist,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RuntimeExecutionPolicy {
    tool_timeout_secs: u64,
    max_flow_depth: usize,
    lua_limits: std::result::Result<LuaLimits, String>,
    json_limits: std::result::Result<JsonLimits, String>,
    lua_http: HttpToolPolicy,
    max_reasoning_bytes: usize,
    chat_history_max_bytes: usize,
    lua_env: LuaEnvPolicy,
    conversation: ConversationTurnPolicy,
}

impl RuntimeExecutionPolicy {
    pub(crate) fn capture() -> Self {
        Self {
            tool_timeout_secs: crate::lua::agent_turn::tool_timeout_secs(),
            max_flow_depth: crate::lua::subflow::max_flow_depth(),
            lua_limits: LuaLimits::from_env().map_err(|error| error.to_string()),
            json_limits: JsonLimits::from_env().map_err(|error| error.to_string()),
            lua_http: HttpToolPolicy::capture(),
            max_reasoning_bytes: crate::llm::provider::max_reasoning_bytes(),
            chat_history_max_bytes: crate::llm::provider::chat_history_max_bytes(),
            lua_env: LuaEnvPolicy::ProcessAllowlist,
            conversation: ConversationTurnPolicy::capture(),
        }
    }

    pub(crate) fn lua_vm_policy(&self) -> Result<LuaVmPolicy> {
        Ok(LuaVmPolicy {
            limits: *self
                .lua_limits
                .as_ref()
                .map_err(|message| IronCrewError::Validation(message.clone()))?,
            json_limits: *self
                .json_limits
                .as_ref()
                .map_err(|message| IronCrewError::Validation(message.clone()))?,
            http: self.lua_http.clone(),
            env: self.lua_env,
        })
    }

    pub(crate) fn tool_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.tool_timeout_secs)
    }

    pub(crate) fn max_flow_depth(&self) -> usize {
        self.max_flow_depth
    }

    pub(crate) fn max_reasoning_bytes(&self) -> usize {
        self.max_reasoning_bytes
    }

    pub(crate) fn chat_history_max_bytes(&self) -> usize {
        self.chat_history_max_bytes
    }

    pub(crate) fn lua_http_policy(&self) -> HttpToolPolicy {
        self.lua_http.clone()
    }

    pub(crate) fn conversation_policy(&self) -> ConversationTurnPolicy {
        self.conversation
    }

    pub(crate) fn block_persistent_lua_env(mut self) -> Self {
        self.lua_env = LuaEnvPolicy::PersistentConversationBlocked;
        self
    }

    pub(crate) fn definition(&self) -> Result<Value> {
        let vm = self.lua_vm_policy()?;
        Ok(json!({
            "tool_timeout_secs": self.tool_timeout_secs,
            "max_flow_depth": self.max_flow_depth,
            "lua": vm.definition(),
            "max_reasoning_bytes": self.max_reasoning_bytes,
            "chat_history_max_bytes": self.chat_history_max_bytes,
            "http_conversation": self.conversation.definition(),
        }))
    }

    #[cfg(test)]
    pub(crate) fn from_values(tool_timeout_secs: u64, max_flow_depth: usize) -> Self {
        Self {
            tool_timeout_secs,
            max_flow_depth,
            lua_limits: Ok(LuaLimits::default()),
            json_limits: Ok(JsonLimits::default()),
            lua_http: HttpToolPolicy::from_values(8_192, false),
            max_reasoning_bytes: crate::llm::provider::DEFAULT_MAX_REASONING_BYTES,
            chat_history_max_bytes: crate::llm::provider::DEFAULT_CHAT_HISTORY_MAX_BYTES,
            lua_env: LuaEnvPolicy::ProcessAllowlist,
            conversation: ConversationTurnPolicy::from_marker(8_192),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_lua_marker(mut self, marker: usize, allow_private: bool) -> Self {
        self.lua_limits = Ok(LuaLimits {
            max_memory_bytes: marker,
            max_instructions: marker as u64,
            max_execution_time: std::time::Duration::from_secs(marker as u64),
        });
        self.json_limits = Ok(JsonLimits::from_marker(marker));
        self.lua_http = HttpToolPolicy::from_values(marker, allow_private);
        self.max_reasoning_bytes = marker;
        self.chat_history_max_bytes = marker;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_conversation_marker(mut self, marker: usize) -> Self {
        self.conversation = ConversationTurnPolicy::from_marker(marker);
        self
    }
}
