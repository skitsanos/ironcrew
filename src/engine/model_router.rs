use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Model purposes for routing.
#[allow(dead_code)]
#[derive(Debug, Clone, Hash, Eq, PartialEq, Serialize, Deserialize)]
pub enum ModelPurpose {
    /// Main task execution (default)
    TaskExecution,
    /// Synthesizing tool outputs back to text
    ToolSynthesis,
    /// Final crew goal summary
    FinalResponse,
    /// Collaborative task discussion turns
    Collaboration,
    /// Collaborative task synthesis
    CollaborationSynthesis,
    /// Custom purpose defined by user
    Custom(String),
}

/// Routes model selection based on purpose.
#[derive(Debug, Clone, Default)]
pub struct ModelRouter {
    routes: HashMap<String, String>, // purpose_key -> model_name
    default_model: Option<String>,
}

impl ModelRouter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Set a model for a purpose.
    pub fn set(&mut self, purpose: &str, model: String) {
        self.routes.insert(purpose.to_string(), model);
    }

    /// Set the default model.
    #[allow(dead_code)]
    pub fn set_default(&mut self, model: String) {
        self.default_model = Some(model);
    }

    /// Resolve the model for a given purpose.
    /// Falls back to: route -> default -> fallback.
    pub fn resolve(&self, purpose: &str, fallback: &str) -> String {
        self.routes
            .get(purpose)
            .or(self.default_model.as_ref())
            .cloned()
            .unwrap_or_else(|| fallback.to_string())
    }

    /// Check if any routes are configured.
    pub fn is_configured(&self) -> bool {
        !self.routes.is_empty() || self.default_model.is_some()
    }
}
