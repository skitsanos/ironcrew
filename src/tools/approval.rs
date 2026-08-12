//! Tool approval gates — human sign-off before a gated tool executes.
//!
//! Sandboxing controls what tools *can* do; this controls what they *may*
//! do per-invocation. The policy is declared per-crew
//! (`require_approval = {...}` in `Crew.new` / `config.lua`) and unioned
//! with the operator-level `IRONCREW_REQUIRE_APPROVAL` env var; it is
//! enforced at the single dispatch choke point (`ToolRegistry::execute`)
//! and rides the existing `InputBridge` — same SSE events, same
//! `questions`/`answer` endpoints, same CLI prompt as `ask_human`, with
//! `kind: "approval"` so UIs can render allow/deny buttons.
//!
//! **Fail closed everywhere:** timeout, missing bridge, and unrecognized
//! answers all deny. Only the exact tokens `allow` / `yes` (once) and
//! `always` (grant for the rest of the flow execution) permit.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::engine::eventbus::CrewEvent;
use crate::engine::input_bridge::AskOutcome;
use crate::tools::ToolCallContext;
use crate::utils::error::Result;

/// Margin added to the approval timeout when computing dispatch deadlines.
pub(crate) const DISPATCH_MARGIN_SECS: u64 = 10;

fn approval_timeout_secs() -> u64 {
    std::env::var("IRONCREW_APPROVAL_TIMEOUT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(crate::engine::input_bridge::default_timeout_secs)
        .max(1)
}

fn args_max_chars() -> usize {
    std::env::var("IRONCREW_APPROVAL_ARGS_MAX_CHARS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(600)
}

/// Which tool calls need a human sign-off, and which the human has already
/// waved through with "always". Registry clones share both via `Arc`, so
/// the policy (and its grants) follows the augmented registries handed to
/// delegated sub-agents, dialogs, and conversations.
#[derive(Clone)]
pub struct ApprovalPolicy {
    /// Exact tool names and `prefix*` globs (entry ending in `*`).
    rules: Arc<Vec<String>>,
    /// Exact non-secret operator limits captured with this policy.
    timeout_secs: u64,
    args_max_chars: usize,
    /// Tool names granted "always" this flow execution.
    granted: Arc<Mutex<HashSet<String>>>,
}

impl ApprovalPolicy {
    /// Build from the crew's `require_approval` list unioned with the
    /// `IRONCREW_REQUIRE_APPROVAL` env var. Returns `None` when both are
    /// empty — no policy, no gate, zero dispatch overhead.
    pub fn from_rules(crew_rules: &[String]) -> Option<Self> {
        let mut rules: Vec<String> = crew_rules
            .iter()
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
        if let Ok(env_rules) = std::env::var("IRONCREW_REQUIRE_APPROVAL") {
            rules.extend(
                env_rules
                    .split(',')
                    .map(|r| r.trim().to_string())
                    .filter(|r| !r.is_empty()),
            );
        }
        Self::from_effective_rules(rules, approval_timeout_secs(), args_max_chars())
    }

    fn from_effective_rules(
        mut rules: Vec<String>,
        timeout_secs: u64,
        args_max_chars: usize,
    ) -> Option<Self> {
        if rules.is_empty() {
            return None;
        }
        rules.sort();
        rules.dedup();
        Some(Self {
            rules: Arc::new(rules),
            timeout_secs,
            args_max_chars,
            granted: Arc::new(Mutex::new(HashSet::new())),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_limits_for_test(
        rules: &[String],
        timeout_secs: u64,
        args_max_chars: usize,
    ) -> Option<Self> {
        Self::from_effective_rules(rules.to_vec(), timeout_secs, args_max_chars)
    }

    pub(crate) fn conversation_definition(&self) -> serde_json::Value {
        serde_json::json!({
            "required": true,
            "timeout_secs": self.timeout_secs,
            "args_max_chars": self.args_max_chars,
        })
    }

    /// Does this tool need approval? `ask_human` is always exempt — gating
    /// the asking channel with a question through the same channel is
    /// noise, not safety.
    pub fn requires(&self, tool: &str) -> bool {
        if tool == "ask_human" {
            return false;
        }
        self.rules.iter().any(|rule| {
            if let Some(prefix) = rule.strip_suffix('*') {
                tool.starts_with(prefix)
            } else {
                rule == tool
            }
        })
    }

    pub fn is_granted(&self, tool: &str) -> bool {
        self.granted
            .lock()
            .expect("approval grants lock poisoned")
            .contains(tool)
    }

    fn grant(&self, tool: &str) {
        self.granted
            .lock()
            .expect("approval grants lock poisoned")
            .insert(tool.to_string());
    }
}

/// Outcome of an approval round-trip.
pub enum Verdict {
    Allow,
    Deny(String),
}

/// Mask values of sensitive-looking keys recursively, so the approval
/// prompt (which lands in the SSE replay buffer) never carries a full
/// bearer token even though the human sees the call's shape.
fn redact(value: &serde_json::Value) -> serde_json::Value {
    const SENSITIVE: &[&str] = &[
        "authorization",
        "token",
        "secret",
        "password",
        "api_key",
        "apikey",
        "cookie",
    ];
    match value {
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(k, v)| {
                    let kl = k.to_lowercase();
                    if SENSITIVE.iter().any(|s| kl.contains(s)) {
                        (k.clone(), serde_json::Value::String("***".into()))
                    } else {
                        (k.clone(), redact(v))
                    }
                })
                .collect(),
        ),
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(redact).collect())
        }
        other => other.clone(),
    }
}

/// Serialized, redacted, length-capped args for the approval prompt.
fn describe_args(args: &serde_json::Value, cap: usize) -> String {
    let mut text = redact(args).to_string();
    if text.len() > cap {
        let mut boundary = cap;
        while boundary > 0 && !text.is_char_boundary(boundary) {
            boundary -= 1;
        }
        text.truncate(boundary);
        text.push_str("… [truncated]");
    }
    text
}

/// Fail-closed verdict parsing: only exact allow tokens permit. Free text
/// denies AND is forwarded to the model as the reason — the human's "no,
/// use the cached data instead" becomes agent steering.
fn parse_answer(tool: &str, value: &serde_json::Value, policy: &ApprovalPolicy) -> Verdict {
    let text = match value {
        serde_json::Value::String(s) => s.trim().to_string(),
        other => other.to_string(),
    };
    match text.to_lowercase().as_str() {
        "allow" | "yes" => Verdict::Allow,
        "always" | "allow-always" => {
            policy.grant(tool);
            Verdict::Allow
        }
        "deny" | "no" | "" => {
            Verdict::Deny(format!("Call to '{}' denied by human operator.", tool))
        }
        _ => Verdict::Deny(format!(
            "Call to '{}' denied by human operator: {}",
            tool, text
        )),
    }
}

/// Run the approval round-trip for a gated tool call. Suspends on the
/// per-run bridge until the human decides (or the timeout denies).
pub async fn request(
    tool: &str,
    args: &serde_json::Value,
    ctx: &ToolCallContext,
    policy: &ApprovalPolicy,
) -> Result<Verdict> {
    // No human channel → a security gate never fails open.
    let Some(ask) = &ctx.ask_human else {
        return Ok(Verdict::Deny(format!(
            "Call to '{}' requires human approval, but no approval channel is \
             available in this execution context.",
            tool
        )));
    };

    let agent = ctx.caller_agent.as_deref().unwrap_or("flow");
    let prompt = format!(
        "[approval] Agent '{}' wants to call {}({}). Allow?",
        agent,
        tool,
        describe_args(args, policy.args_max_chars)
    );
    let choices = vec![
        "allow".to_string(),
        "always".to_string(),
        "deny".to_string(),
    ];
    let timeout_s = policy.timeout_secs;

    let eventbus = ask.eventbus.clone().or_else(|| ctx.eventbus.clone());
    let store = ask.store.clone().or_else(|| ctx.store.clone());
    let question_id = uuid::Uuid::new_v4().to_string();

    let requested_event = CrewEvent::HumanInputRequested {
        question_id: question_id.clone(),
        prompt: prompt.clone(),
        choices: choices.clone(),
        timeout_s,
        kind: "approval".into(),
    };
    let requested_eventbus = eventbus.clone();
    let outcome = ask
        .bridge
        .with_run_wait_status(
            store.clone(),
            ask.run_id.as_deref(),
            ask.bridge.ask_when_ready(
                &question_id,
                &prompt,
                &choices,
                timeout_s,
                "approval",
                move || {
                    if let Some(bus) = requested_eventbus {
                        bus.emit(requested_event);
                    }
                },
            ),
        )
        .await?;

    let (verdict, outcome_str) = match outcome {
        AskOutcome::Answered(value) => (parse_answer(tool, &value, policy), "answered"),
        AskOutcome::TimedOut => (
            Verdict::Deny(format!(
                "Call to '{}' denied: no approval received within {}s.",
                tool, timeout_s
            )),
            "timeout",
        ),
    };
    if let Some(bus) = &eventbus {
        bus.emit(CrewEvent::HumanInputReceived {
            question_id,
            outcome: outcome_str.into(),
        });
    }
    Ok(verdict)
}

/// Dispatch-deadline allowance for a gated-and-not-yet-granted call, so the
/// generic tool timeout can't kill a legitimate approval wait.
pub(crate) fn gate_dispatch_allowance(
    policy: &ApprovalPolicy,
    tool: &str,
) -> Option<std::time::Duration> {
    if policy.requires(tool) && !policy.is_granted(tool) {
        Some(std::time::Duration::from_secs(
            policy.timeout_secs + DISPATCH_MARGIN_SECS,
        ))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(rules: &[&str]) -> ApprovalPolicy {
        ApprovalPolicy::from_rules(&rules.iter().map(|s| s.to_string()).collect::<Vec<_>>())
            .expect("non-empty rules")
    }

    #[test]
    fn exact_and_glob_matching() {
        let p = policy(&["http_request", "mcp__git__*"]);
        assert!(p.requires("http_request"));
        assert!(p.requires("mcp__git__git_push"));
        assert!(!p.requires("mcp__jira__create"));
        assert!(!p.requires("file_read"));
    }

    #[test]
    fn star_gates_everything_except_ask_human() {
        let p = policy(&["*"]);
        assert!(p.requires("file_write"));
        assert!(p.requires("agent__deployer"));
        assert!(!p.requires("ask_human"), "ask_human is always exempt");
    }

    #[test]
    fn empty_rules_mean_no_policy() {
        assert!(ApprovalPolicy::from_rules(&[]).is_none());
        assert!(ApprovalPolicy::from_rules(&["  ".into()]).is_none());
    }

    #[test]
    fn grants_are_shared_across_clones() {
        let p = policy(&["http_request"]);
        let clone = p.clone();
        assert!(!clone.is_granted("http_request"));
        p.grant("http_request");
        assert!(clone.is_granted("http_request"), "Arc-shared grant set");
    }

    #[test]
    fn answer_parsing_is_fail_closed() {
        let p = policy(&["shell"]);
        assert!(matches!(
            parse_answer("shell", &serde_json::json!("allow"), &p),
            Verdict::Allow
        ));
        assert!(matches!(
            parse_answer("shell", &serde_json::json!("YES"), &p),
            Verdict::Allow
        ));
        // "always" allows AND grants.
        assert!(matches!(
            parse_answer("shell", &serde_json::json!("always"), &p),
            Verdict::Allow
        ));
        assert!(p.is_granted("shell"));
        // Everything else denies — free text becomes the reason.
        match parse_answer("shell", &serde_json::json!("use the cached data"), &p) {
            Verdict::Deny(reason) => assert!(reason.contains("use the cached data")),
            _ => panic!("free text must deny"),
        }
        assert!(matches!(
            parse_answer("shell", &serde_json::json!({"weird": true}), &p),
            Verdict::Deny(_)
        ));
        assert!(matches!(
            parse_answer("shell", &serde_json::json!("deny"), &p),
            Verdict::Deny(_)
        ));
    }

    #[test]
    fn redaction_masks_sensitive_keys_recursively() {
        let args = serde_json::json!({
            "url": "https://api.example.com",
            "headers": { "Authorization": "Bearer sk-live-12345", "X-Trace": "ok" },
            "nested": [{ "api_key": "k-999" }],
        });
        let out = redact(&args);
        let text = out.to_string();
        assert!(!text.contains("sk-live-12345"));
        assert!(!text.contains("k-999"));
        assert!(text.contains("***"));
        assert!(text.contains("https://api.example.com"));
        assert!(text.contains("ok"));
    }

    #[test]
    fn describe_args_caps_length() {
        let big = serde_json::json!({ "data": "x".repeat(5000) });
        let text = describe_args(&big, 600);
        assert!(text.len() < 5000);
        assert!(text.ends_with("… [truncated]"));
    }
}
