pub mod anthropic;
pub(crate) mod execution_policy;
pub mod image;
pub(crate) mod metrics;
pub mod openai;
pub mod openai_responses;
pub mod provider;

/// Default OpenAI model used when a crew does not select one explicitly.
pub(crate) const DEFAULT_OPENAI_MODEL: &str = "gpt-5.6-luna";
