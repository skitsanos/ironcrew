use super::{ModelPolicy, Temperature};

// Verified documented efforts: GPT-5.6 guide and Luna model page, 2026-09-20.
// Luna temperature/tool restrictions retain the IC-042 live-observed contract;
// the public model page does not enumerate those two restrictions.
const GPT_56_EFFORTS: &[&str] = &["none", "low", "medium", "high", "xhigh", "max"];

pub(super) const OPENAI: &[ModelPolicy] = &[
    ModelPolicy {
        aliases: &["gpt-5.6-luna"],
        efforts: Some(GPT_56_EFFORTS),
        default_effort: Some("low"),
        temperature: Temperature::DefaultOnly,
        chat_tools_require_none: true,
    },
    ModelPolicy {
        aliases: &["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra"],
        efforts: Some(GPT_56_EFFORTS),
        default_effort: None,
        temperature: Temperature::ProviderChecked,
        chat_tools_require_none: false,
    },
];

// Anthropic's manual-thinking migration table, verified 2026-09-20.
pub(super) const ANTHROPIC_ADAPTIVE_ONLY: &[&str] = &[
    "claude-opus-4-7",
    "claude-opus-4-8",
    "claude-opus-5",
    "claude-sonnet-5",
    "claude-fable-5",
    "claude-fable-5-1",
    "claude-mythos-5",
    "claude-mythos-5-1",
];
