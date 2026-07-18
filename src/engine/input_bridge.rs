//! Human-to-flow input transport for `crew:ask_human()`.
//!
//! One `InputBridge` exists per run. The flow coroutine registers a question
//! and parks on a oneshot receiver; the human's answer arrives either over
//! HTTP (`POST /flows/{flow}/answer/{run_id}` resolves the oneshot) or on the
//! terminal (CLI mode prompts stdin directly). The bridge is transport only —
//! event emission and run-status transitions belong to the caller.
//!
//! Deliberately orthogonal to the agent-to-agent `MessageBus`: the bus is a
//! non-blocking mailbox drained at turn boundaries, while `ask_human` needs
//! blocking mid-turn suspension with an external wake-up (see the ask_human
//! design spec §11).

use std::collections::HashMap;
use std::io::IsTerminal;
use std::sync::Mutex;

use serde::Serialize;
use tokio::sync::oneshot;

use crate::utils::error::{IronCrewError, Result};

/// Default per-question timeout (seconds) when the flow omits `timeout_s`.
const DEFAULT_TIMEOUT_SECS: u64 = 600;

/// Default cap on simultaneously pending questions per run — a runaway
/// `foreach_parallel` guard, not a workflow limit.
const DEFAULT_MAX_PENDING: usize = 16;
const DEFAULT_MAX_TIMEOUT_SECS: u64 = 3_600;
const DEFAULT_MAX_PROMPT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_CHOICES: usize = 100;
const DEFAULT_MAX_CHOICES_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_ANSWER_BYTES: usize = 64 * 1024;
const HARD_MAX_TIMEOUT_SECS: u64 = 86_400;
const HARD_MAX_PENDING: usize = 256;
const HARD_MAX_PROMPT_BYTES: usize = 1024 * 1024;
const HARD_MAX_CHOICES: usize = 1_000;
const HARD_MAX_CHOICES_BYTES: usize = 1024 * 1024;
const HARD_MAX_ANSWER_BYTES: usize = 1024 * 1024;

fn positive_env_limit<T>(name: &str, default: T, hard_max: T) -> T
where
    T: std::str::FromStr + PartialOrd + From<u8> + Copy,
{
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<T>().ok())
        .filter(|value| *value > T::from(0))
        .filter(|value| *value <= hard_max)
        .unwrap_or(default)
}

pub fn default_timeout_secs() -> u64 {
    positive_env_limit(
        "IRONCREW_ASK_HUMAN_TIMEOUT",
        DEFAULT_TIMEOUT_SECS,
        HARD_MAX_TIMEOUT_SECS,
    )
    .min(max_timeout_secs())
}

fn max_timeout_secs() -> u64 {
    positive_env_limit(
        "IRONCREW_ASK_HUMAN_MAX_TIMEOUT",
        DEFAULT_MAX_TIMEOUT_SECS,
        HARD_MAX_TIMEOUT_SECS,
    )
}

fn max_pending() -> usize {
    positive_env_limit(
        "IRONCREW_ASK_HUMAN_MAX_PENDING",
        DEFAULT_MAX_PENDING,
        HARD_MAX_PENDING,
    )
}

/// Where answers come from. Decided once per process entrypoint.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BridgeMode {
    /// `ironcrew serve` — answers arrive via the answer endpoint.
    Http,
    /// `ironcrew run` — prompt on stderr, read one line from stdin.
    Tty,
}

/// Public metadata for a pending question (everything but the wake-up
/// channel) — the shape returned by `GET /flows/{flow}/questions/{run_id}`.
#[derive(Debug, Clone, Serialize)]
pub struct QuestionInfo {
    pub question_id: String,
    pub prompt: String,
    pub choices: Vec<String>,
    pub asked_at: String,
    pub timeout_s: u64,
    /// `"question"` (ask_human) or `"approval"` (tool approval gate) — lets
    /// UIs render an answer form vs allow/deny buttons off one field.
    pub kind: String,
}

struct PendingQuestion {
    info: QuestionInfo,
    tx: oneshot::Sender<serde_json::Value>,
}

/// How a question resolved, from the asker's point of view.
#[derive(Debug)]
pub enum AskOutcome {
    Answered(serde_json::Value),
    TimedOut,
}

/// Per-run handle for human-input suspension. Threaded into the Lua VM as
/// app data (for the `crew:ask_human` flow primitive) and into
/// `ToolCallContext.ask_human` (for the agent-facing `ask_human` tool).
/// Carries everything a suspension point needs — bridge, run identity,
/// telemetry, store — so exactly one optional value travels through the
/// crew-run call chain instead of three.
#[derive(Clone)]
pub struct AskHumanContext {
    pub bridge: std::sync::Arc<InputBridge>,
    /// Persisted run id (`save_run_intent`) when this execution is tracked;
    /// `None` for contexts without a run record. `crew:run()` re-binds this
    /// to the actual run id it allocates, so agent-initiated asks flip the
    /// real record even in CLI mode.
    pub run_id: Option<String>,
    /// Store for the best-effort `Running ↔ WaitingForInput` flips.
    pub store: Option<std::sync::Arc<dyn crate::engine::store::StateStore>>,
    /// Bus for `HumanInputRequested` / `HumanInputReceived`. The crew-run
    /// executor's tool loop carries no bus of its own, so the tool relies
    /// on this one.
    pub eventbus: Option<crate::engine::eventbus::EventBus>,
}

pub struct InputBridge {
    mode: BridgeMode,
    pending: Mutex<HashMap<String, PendingQuestion>>,
}

impl InputBridge {
    pub fn new(mode: BridgeMode) -> Self {
        Self {
            mode,
            pending: Mutex::new(HashMap::new()),
        }
    }

    /// Snapshot of pending questions, oldest first — for the questions
    /// endpoint and for UIs recovering state after a missed SSE event.
    pub fn list(&self) -> Vec<QuestionInfo> {
        let map = self.pending.lock().expect("input bridge lock poisoned");
        let mut infos: Vec<QuestionInfo> = map.values().map(|q| q.info.clone()).collect();
        infos.sort_by(|a, b| a.asked_at.cmp(&b.asked_at));
        infos
    }

    pub fn pending_count(&self) -> usize {
        self.pending
            .lock()
            .expect("input bridge lock poisoned")
            .len()
    }

    /// Deliver a human answer to a pending question. First writer wins: the
    /// question is removed before the send, so a concurrent second answer
    /// sees "unknown or expired" instead of silently overwriting.
    pub fn answer(&self, question_id: &str, value: serde_json::Value) -> Result<()> {
        struct Counter(usize);
        impl std::io::Write for Counter {
            fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
                self.0 = self.0.saturating_add(buffer.len());
                Ok(buffer.len())
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut counter = Counter(0);
        serde_json::to_writer(&mut counter, &value).map_err(|error| {
            IronCrewError::Validation(format!("Invalid question answer: {error}"))
        })?;
        let max_answer = positive_env_limit(
            "IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES",
            DEFAULT_MAX_ANSWER_BYTES,
            HARD_MAX_ANSWER_BYTES,
        );
        if counter.0 > max_answer {
            return Err(IronCrewError::Validation(format!(
                "Question answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES ({max_answer})"
            )));
        }
        let q = self
            .pending
            .lock()
            .expect("input bridge lock poisoned")
            .remove(question_id)
            .ok_or_else(|| {
                IronCrewError::Validation(format!("Unknown or expired question '{}'", question_id))
            })?;
        // Send fails only if the asker gave up (timed out) between our
        // remove and this send — surface that as expired, not success.
        q.tx.send(value).map_err(|_| {
            IronCrewError::Validation(format!(
                "Question '{}' expired before the answer was delivered",
                question_id
            ))
        })
    }

    /// Ask a question and suspend until it resolves. Emitting events and
    /// flipping the run status around this call is the caller's job.
    /// `kind` is `"question"` or `"approval"` (see `QuestionInfo::kind`).
    pub async fn ask(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
    ) -> Result<AskOutcome> {
        let max_timeout = max_timeout_secs();
        if timeout_s == 0 || timeout_s > max_timeout {
            return Err(IronCrewError::Validation(format!(
                "ask_human timeout must be between 1 and {max_timeout} seconds"
            )));
        }
        let max_prompt = positive_env_limit(
            "IRONCREW_ASK_HUMAN_MAX_PROMPT_BYTES",
            DEFAULT_MAX_PROMPT_BYTES,
            HARD_MAX_PROMPT_BYTES,
        );
        if prompt.len() > max_prompt {
            return Err(IronCrewError::Validation(format!(
                "ask_human prompt exceeds IRONCREW_ASK_HUMAN_MAX_PROMPT_BYTES ({max_prompt})"
            )));
        }
        let max_choices = positive_env_limit(
            "IRONCREW_ASK_HUMAN_MAX_CHOICES",
            DEFAULT_MAX_CHOICES,
            HARD_MAX_CHOICES,
        );
        if choices.len() > max_choices {
            return Err(IronCrewError::Validation(format!(
                "ask_human choices exceed IRONCREW_ASK_HUMAN_MAX_CHOICES ({max_choices})"
            )));
        }
        let max_choice_bytes = positive_env_limit(
            "IRONCREW_ASK_HUMAN_MAX_CHOICES_BYTES",
            DEFAULT_MAX_CHOICES_BYTES,
            HARD_MAX_CHOICES_BYTES,
        );
        let choice_bytes = choices
            .iter()
            .try_fold(0usize, |total, choice| total.checked_add(choice.len()));
        if choice_bytes.is_none_or(|total| total > max_choice_bytes) {
            return Err(IronCrewError::Validation(format!(
                "ask_human choices exceed IRONCREW_ASK_HUMAN_MAX_CHOICES_BYTES ({max_choice_bytes})"
            )));
        }
        match self.mode {
            BridgeMode::Http => {
                self.ask_http(question_id, prompt, choices, timeout_s, kind)
                    .await
            }
            BridgeMode::Tty => ask_tty(prompt, choices, timeout_s).await,
        }
    }

    async fn ask_http(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
    ) -> Result<AskOutcome> {
        let rx = self.register(question_id, prompt, choices, timeout_s, kind)?;

        let outcome = tokio::select! {
            answer = rx => match answer {
                Ok(value) => AskOutcome::Answered(value),
                // Sender dropped without a send — only possible if the
                // question was force-removed; treat as timeout.
                Err(_) => AskOutcome::TimedOut,
            },
            _ = tokio::time::sleep(std::time::Duration::from_secs(timeout_s)) => AskOutcome::TimedOut,
        };

        // On timeout the entry is still registered — clean it up so a late
        // answer gets a 404 instead of feeding a completed question.
        if matches!(outcome, AskOutcome::TimedOut) {
            self.pending
                .lock()
                .expect("input bridge lock poisoned")
                .remove(question_id);
        }
        Ok(outcome)
    }

    fn register(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
    ) -> Result<oneshot::Receiver<serde_json::Value>> {
        if question_id.is_empty()
            || question_id.len() > 128
            || question_id.chars().any(char::is_control)
        {
            return Err(IronCrewError::Validation(
                "Question id must be 1-128 printable characters".into(),
            ));
        }
        if !matches!(kind, "question" | "approval") {
            return Err(IronCrewError::Validation(
                "Question kind must be 'question' or 'approval'".into(),
            ));
        }
        let mut map = self.pending.lock().expect("input bridge lock poisoned");
        if map.contains_key(question_id) {
            return Err(IronCrewError::Validation(format!(
                "Question '{}' is already pending",
                question_id
            )));
        }
        if map.len() >= max_pending() {
            return Err(IronCrewError::Validation(format!(
                "Too many pending questions ({}); raise IRONCREW_ASK_HUMAN_MAX_PENDING if intentional",
                map.len()
            )));
        }
        let (tx, rx) = oneshot::channel();
        map.insert(
            question_id.to_string(),
            PendingQuestion {
                info: QuestionInfo {
                    question_id: question_id.to_string(),
                    prompt: prompt.to_string(),
                    choices: choices.to_vec(),
                    asked_at: chrono::Utc::now().to_rfc3339(),
                    timeout_s,
                    kind: kind.to_string(),
                },
                tx,
            },
        );
        Ok(rx)
    }
}

/// CLI-mode ask: prompt on stderr (stdout may carry flow output), read one
/// line from stdin via `spawn_blocking`, same timeout semantics as HTTP.
/// Non-TTY stdin (piped/CI) resolves as an immediate timeout so unattended
/// runs fall through to `default` or a clean Lua error instead of hanging.
async fn ask_tty(prompt: &str, choices: &[String], timeout_s: u64) -> Result<AskOutcome> {
    if !std::io::stdin().is_terminal() {
        return Ok(AskOutcome::TimedOut);
    }

    eprintln!();
    eprintln!("── ask_human ──────────────────────────────");
    eprintln!("{}", prompt);
    for (i, c) in choices.iter().enumerate() {
        eprintln!("  {}. {}", i + 1, c);
    }
    if choices.is_empty() {
        eprint!("> ");
    } else {
        eprint!("(number or free text) > ");
    }

    let choices = choices.to_vec();
    let read = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) => None, // EOF
            Ok(_) => {
                let trimmed = line.trim().to_string();
                // A bare number selecting a listed choice returns the choice
                // text, so flows compare against their own strings.
                if let Ok(n) = trimmed.parse::<usize>()
                    && n >= 1
                    && n <= choices.len()
                {
                    return Some(choices[n - 1].clone());
                }
                Some(trimmed)
            }
            Err(_) => None,
        }
    });

    match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), read).await {
        Ok(Ok(Some(text))) => Ok(AskOutcome::Answered(serde_json::Value::String(text))),
        Ok(Ok(None)) | Err(_) => Ok(AskOutcome::TimedOut),
        Ok(Err(e)) => Err(IronCrewError::Validation(format!(
            "stdin reader task failed: {}",
            e
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bridge() -> InputBridge {
        InputBridge::new(BridgeMode::Http)
    }

    #[tokio::test]
    async fn answer_resolves_pending_ask() {
        let b = std::sync::Arc::new(bridge());
        let b2 = b.clone();
        let ask = tokio::spawn(async move {
            b2.ask(
                "q1",
                "Proceed?",
                &["yes".into(), "no".into()],
                30,
                "question",
            )
            .await
        });

        // Wait until the question is registered, then answer.
        while b.pending_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert_eq!(b.list()[0].prompt, "Proceed?");
        b.answer("q1", json!("yes")).unwrap();

        match ask.await.unwrap().unwrap() {
            AskOutcome::Answered(v) => assert_eq!(v, json!("yes")),
            other => panic!("expected Answered, got {:?}", other),
        }
        assert_eq!(b.pending_count(), 0);
    }

    #[tokio::test]
    async fn zero_timeout_is_rejected_without_registering_question() {
        let b = bridge();
        let outcome = b.ask("q1", "Anyone there?", &[], 0, "question").await;
        assert!(outcome.is_err());
        assert_eq!(b.pending_count(), 0);
        assert!(b.answer("q1", json!("too late")).is_err());
    }

    #[tokio::test]
    async fn oversized_prompt_and_choices_are_rejected() {
        let b = bridge();
        let prompt = "x".repeat(DEFAULT_MAX_PROMPT_BYTES + 1);
        assert!(b.ask("q1", &prompt, &[], 30, "question").await.is_err());

        let choices = vec!["x".to_string(); DEFAULT_MAX_CHOICES + 1];
        assert!(b.ask("q2", "Pick", &choices, 30, "question").await.is_err());
    }

    #[tokio::test]
    async fn unknown_question_id_errors() {
        let b = bridge();
        assert!(b.answer("nope", json!(1)).is_err());
    }

    #[tokio::test]
    async fn double_answer_second_writer_loses() {
        let b = std::sync::Arc::new(bridge());
        let b2 = b.clone();
        let ask = tokio::spawn(async move { b2.ask("q1", "Pick", &[], 30, "question").await });
        while b.pending_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        b.answer("q1", json!("first")).unwrap();
        assert!(b.answer("q1", json!("second")).is_err());
        match ask.await.unwrap().unwrap() {
            AskOutcome::Answered(v) => assert_eq!(v, json!("first")),
            other => panic!("expected Answered, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn pending_cap_enforced() {
        let b = std::sync::Arc::new(bridge());
        // Park DEFAULT_MAX_PENDING askers without answering.
        let mut handles = Vec::new();
        for i in 0..DEFAULT_MAX_PENDING {
            let b2 = b.clone();
            handles.push(tokio::spawn(async move {
                let _ = b2
                    .ask(&format!("q{}", i), "wait", &[], 30, "question")
                    .await;
            }));
        }
        while b.pending_count() < DEFAULT_MAX_PENDING {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        // One more trips the guard.
        let err = b.ask("overflow", "one too many", &[], 30, "question").await;
        assert!(err.is_err());
        // Unblock the parked askers.
        for i in 0..DEFAULT_MAX_PENDING {
            let _ = b.answer(&format!("q{}", i), json!("ok"));
        }
        for h in handles {
            let _ = h.await;
        }
    }

    #[tokio::test]
    async fn concurrent_questions_answered_out_of_order() {
        // Two parallel branches each ask; answering the SECOND question
        // first must resume the right asker (spec §9.8).
        let b = std::sync::Arc::new(bridge());
        let b1 = b.clone();
        let ask_a =
            tokio::spawn(async move { b1.ask("qa", "branch A", &[], 30, "question").await });
        let b2 = b.clone();
        let ask_b =
            tokio::spawn(async move { b2.ask("qb", "branch B", &[], 30, "question").await });

        while b.pending_count() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        b.answer("qb", json!("answer-b")).unwrap();
        b.answer("qa", json!("answer-a")).unwrap();

        match ask_a.await.unwrap().unwrap() {
            AskOutcome::Answered(v) => assert_eq!(v, json!("answer-a")),
            other => panic!("branch A: expected Answered, got {:?}", other),
        }
        match ask_b.await.unwrap().unwrap() {
            AskOutcome::Answered(v) => assert_eq!(v, json!("answer-b")),
            other => panic!("branch B: expected Answered, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn list_is_metadata_only_and_ordered() {
        let b = std::sync::Arc::new(bridge());
        for (i, id) in ["a", "b"].iter().enumerate() {
            let b2 = b.clone();
            let id = id.to_string();
            tokio::spawn(async move {
                let _ = b2
                    .ask(&id, &format!("q #{}", i), &["x".into()], 30, "question")
                    .await;
            });
        }
        while b.pending_count() < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let infos = b.list();
        assert_eq!(infos.len(), 2);
        assert!(infos[0].asked_at <= infos[1].asked_at);
        assert_eq!(infos[0].choices, vec!["x".to_string()]);
        let _ = b.answer("a", json!(1));
        let _ = b.answer("b", json!(2));
    }
}
