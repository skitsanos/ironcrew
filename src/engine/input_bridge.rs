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
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

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

fn max_answer_bytes() -> usize {
    positive_env_limit(
        "IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES",
        DEFAULT_MAX_ANSWER_BYTES,
        HARD_MAX_ANSWER_BYTES,
    )
}

/// Why an HTTP-delivered answer was rejected. Keeping payload validation
/// distinct from question lookup lets transports return an accurate 4xx
/// status without parsing error strings.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum AnswerError {
    #[error("Invalid question answer: {0}")]
    Invalid(String),
    #[error("Question answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES ({max_bytes})")]
    TooLarge { max_bytes: usize },
    #[error("Unknown or expired question '{question_id}'")]
    UnknownOrExpired { question_id: String },
}

fn validate_http_answer_size(value: &serde_json::Value) -> std::result::Result<(), AnswerError> {
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
    serde_json::to_writer(&mut counter, value)
        .map_err(|error| AnswerError::Invalid(error.to_string()))?;
    let max_answer = max_answer_bytes();
    if counter.0 > max_answer {
        return Err(AnswerError::TooLarge {
            max_bytes: max_answer,
        });
    }
    Ok(())
}

fn validate_answer_size(value: &serde_json::Value) -> Result<()> {
    validate_http_answer_size(value).map_err(|error| IronCrewError::Validation(error.to_string()))
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
    generation: u64,
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
    /// Only one attended terminal reader may own stdin at a time. HTTP mode
    /// never acquires this lock and retains fully concurrent questions.
    tty_prompt: tokio::sync::Mutex<()>,
    /// Count every status-tracked waiter from before its durable
    /// `WaitingForInput` write until after its question resolves. This is
    /// deliberately broader than `pending`: registration happens only after
    /// the initial async status write.
    run_status_waiters: Mutex<usize>,
    /// Serialize durable status writes for this run. Each writer chooses the
    /// desired state only after acquiring the lock, so a queued transition
    /// always observes the freshest waiter count.
    run_status_updates: tokio::sync::Mutex<()>,
    /// Distinguishes a cancelled registration from a later question that
    /// deliberately reuses the same public id.
    next_question_generation: AtomicU64,
    expired: AtomicBool,
}

#[derive(Clone)]
struct RunStatusTarget {
    store: std::sync::Arc<dyn crate::engine::store::StateStore>,
    run_id: String,
}

struct RunStatusWait {
    bridge: std::sync::Arc<InputBridge>,
    target: Option<RunStatusTarget>,
    counted: bool,
}

impl RunStatusWait {
    fn finish_wait(&mut self) {
        if self.counted {
            // Clear the flag first so an invariant panic cannot make Drop try
            // to decrement the same waiter a second time during unwinding.
            self.counted = false;
            self.bridge.finish_run_status_wait();
        }
    }

    fn disarm(mut self) {
        self.finish_wait();
        self.target = None;
    }
}

impl Drop for RunStatusWait {
    fn drop(&mut self) {
        self.finish_wait();
        let Some(target) = self.target.take() else {
            return;
        };
        let bridge = self.bridge.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                // Whole-run termination owns the final durable transition; a
                // detached branch cleanup must not compete with it.
                if !bridge.is_expired() {
                    bridge
                        .sync_run_status(target.store.as_ref(), &target.run_id)
                        .await;
                }
            });
        }
    }
}

struct PendingRegistration<'a> {
    bridge: &'a InputBridge,
    question_id: &'a str,
    generation: u64,
}

impl Drop for PendingRegistration<'_> {
    fn drop(&mut self) {
        let mut pending = self
            .bridge
            .pending
            .lock()
            .expect("input bridge lock poisoned");
        if pending
            .get(self.question_id)
            .is_some_and(|question| question.generation == self.generation)
        {
            pending.remove(self.question_id);
        }
    }
}

impl InputBridge {
    pub fn new(mode: BridgeMode) -> Self {
        Self {
            mode,
            pending: Mutex::new(HashMap::new()),
            tty_prompt: tokio::sync::Mutex::new(()),
            run_status_waiters: Mutex::new(0),
            run_status_updates: tokio::sync::Mutex::new(()),
            next_question_generation: AtomicU64::new(1),
            expired: AtomicBool::new(false),
        }
    }

    fn begin_run_status_wait(
        self: &std::sync::Arc<Self>,
        target: Option<RunStatusTarget>,
    ) -> RunStatusWait {
        let mut waiters = self
            .run_status_waiters
            .lock()
            .expect("input bridge status lock poisoned");
        *waiters = waiters
            .checked_add(1)
            .expect("input bridge status waiter count overflowed");
        RunStatusWait {
            bridge: self.clone(),
            target,
            counted: true,
        }
    }

    fn finish_run_status_wait(&self) {
        let mut waiters = self
            .run_status_waiters
            .lock()
            .expect("input bridge status lock poisoned");
        *waiters = waiters
            .checked_sub(1)
            .expect("input bridge status waiter count underflowed");
    }

    async fn sync_run_status_with<F, Fut>(&self, update: F) -> Result<()>
    where
        F: FnOnce(crate::engine::run_history::RunStatus) -> Fut,
        Fut: Future<Output = Result<()>>,
    {
        let _update_guard = self.run_status_updates.lock().await;
        let has_waiters = *self
            .run_status_waiters
            .lock()
            .expect("input bridge status lock poisoned")
            > 0;
        let status = if has_waiters {
            crate::engine::run_history::RunStatus::WaitingForInput
        } else {
            crate::engine::run_history::RunStatus::Running
        };
        update(status).await
    }

    async fn sync_run_status(&self, store: &dyn crate::engine::store::StateStore, run_id: &str) {
        if let Err(error) = self
            .sync_run_status_with(|status| store.update_run_status(run_id, status))
            .await
        {
            // Best effort: transport remains authoritative when a standalone
            // ask has no persisted run or a terminal transition won the race.
            tracing::debug!(%error, "Human-input run status was not updated");
        }
    }

    /// Run one human suspension with race-free durable status transitions.
    ///
    /// The active-waiter marker is installed before the first async store
    /// write. Both the entry and exit writes are serialized and decide between
    /// `WaitingForInput` and `Running` only after acquiring that serializer.
    /// Thus a sibling that starts while another branch is restoring `Running`
    /// necessarily queues a later `WaitingForInput` write, while a sibling that
    /// finishes during a queued waiting write queues a later `Running` write.
    pub(crate) async fn with_run_wait_status<F, T>(
        self: &std::sync::Arc<Self>,
        store: Option<std::sync::Arc<dyn crate::engine::store::StateStore>>,
        run_id: Option<&str>,
        wait: F,
    ) -> T
    where
        F: Future<Output = T>,
    {
        let target = store.zip(run_id).map(|(store, run_id)| RunStatusTarget {
            store,
            run_id: run_id.to_string(),
        });
        let mut status_wait = self.begin_run_status_wait(target.clone());
        if let Some(target) = target.as_ref() {
            self.sync_run_status(target.store.as_ref(), &target.run_id)
                .await;
        }

        let outcome = wait.await;
        status_wait.finish_wait();

        if let Some(target) = target.as_ref() {
            self.sync_run_status(target.store.as_ref(), &target.run_id)
                .await;
        }
        status_wait.disarm();
        outcome
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

    /// Permanently close this run's question transport and drop every pending
    /// sender. Holding the pending lock while marking the bridge expired makes
    /// this atomic with registration: a concurrent asker is either removed
    /// here or observes the expired state before it can register.
    pub fn expire_all(&self) -> usize {
        let mut pending = self.pending.lock().expect("input bridge lock poisoned");
        self.expired.store(true, Ordering::Release);
        let expired = pending.len();
        pending.clear();
        expired
    }

    /// Whether the run has permanently closed this question transport.
    pub fn is_expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }

    /// Deliver a human answer to a pending question. First writer wins: the
    /// question is removed before the send, so a concurrent second answer
    /// sees "unknown or expired" instead of silently overwriting.
    pub fn answer(
        &self,
        question_id: &str,
        value: serde_json::Value,
    ) -> std::result::Result<(), AnswerError> {
        validate_http_answer_size(&value)?;
        let q = self
            .pending
            .lock()
            .expect("input bridge lock poisoned")
            .remove(question_id)
            .ok_or_else(|| AnswerError::UnknownOrExpired {
                question_id: question_id.to_string(),
            })?;
        // Send fails only if the asker gave up (timed out) between our
        // remove and this send — surface that as expired, not success.
        q.tx.send(value).map_err(|_| AnswerError::UnknownOrExpired {
            question_id: question_id.to_string(),
        })
    }

    /// Ask a question and suspend until it resolves. Emitting events and
    /// flipping the run status around this call is the caller's job.
    /// `kind` is `"question"` or `"approval"` (see `QuestionInfo::kind`).
    #[allow(dead_code)] // Public library API; production call sites also publish a ready event.
    pub async fn ask(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
    ) -> Result<AskOutcome> {
        self.ask_when_ready(question_id, prompt, choices, timeout_s, kind, || {})
            .await
    }

    /// Ask a question and invoke `on_ready` only after the selected transport
    /// can accept its answer. HTTP callers use this hook to publish
    /// `HumanInputRequested` without creating an event-to-answer race: by the
    /// time a subscriber sees the event, `answer()` can already resolve the
    /// registered question.
    pub(crate) async fn ask_when_ready<F>(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
        on_ready: F,
    ) -> Result<AskOutcome>
    where
        F: FnOnce(),
    {
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
                self.ask_http(question_id, prompt, choices, timeout_s, kind, on_ready)
                    .await
            }
            BridgeMode::Tty => {
                let _prompt_guard = self.tty_prompt.lock().await;
                on_ready();
                ask_tty(prompt, choices, timeout_s).await
            }
        }
    }

    async fn ask_http(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
        on_ready: impl FnOnce(),
    ) -> Result<AskOutcome> {
        let (rx, generation) = self.register(question_id, prompt, choices, timeout_s, kind)?;
        let _registration = PendingRegistration {
            bridge: self,
            question_id,
            generation,
        };
        on_ready();

        Ok(await_http_answer(rx, std::time::Duration::from_secs(timeout_s)).await)
    }

    fn register(
        &self,
        question_id: &str,
        prompt: &str,
        choices: &[String],
        timeout_s: u64,
        kind: &str,
    ) -> Result<(oneshot::Receiver<serde_json::Value>, u64)> {
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
        if self.expired.load(Ordering::Acquire) {
            return Err(IronCrewError::Validation(
                "Question transport has expired".into(),
            ));
        }
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
        let generation = self
            .next_question_generation
            .fetch_add(1, Ordering::Relaxed);
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
                generation,
            },
        );
        Ok((rx, generation))
    }
}

async fn await_http_answer(
    rx: oneshot::Receiver<serde_json::Value>,
    timeout: std::time::Duration,
) -> AskOutcome {
    tokio::select! {
        // If delivery and the deadline become observable in the same poll,
        // an answer already accepted by `answer()` deterministically wins.
        biased;
        answer = rx => match answer {
            Ok(value) => AskOutcome::Answered(value),
            // Sender dropped without a send — only possible if the question
            // was force-removed; treat as timeout.
            Err(_) => AskOutcome::TimedOut,
        },
        _ = tokio::time::sleep(timeout) => AskOutcome::TimedOut,
    }
}

/// CLI-mode ask: prompt on stderr (stdout may carry flow output), read one
/// line from the controlling terminal, with the same timeout semantics as HTTP.
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
    std::io::stderr()
        .flush()
        .map_err(|error| IronCrewError::Validation(format!("stderr flush failed: {error}")))?;

    let choices = choices.to_vec();
    match read_tty_line(
        max_answer_bytes(),
        std::time::Duration::from_secs(timeout_s),
    )
    .await
    {
        Ok(Some(line)) => {
            let trimmed = line.trim().to_string();
            // A bare number selecting a listed choice returns the choice
            // text, so flows compare against their own strings.
            if let Ok(n) = trimmed.parse::<usize>()
                && n >= 1
                && n <= choices.len()
            {
                let answer = serde_json::Value::String(choices[n - 1].clone());
                validate_answer_size(&answer)?;
                return Ok(AskOutcome::Answered(answer));
            }
            let answer = serde_json::Value::String(trimmed);
            validate_answer_size(&answer)?;
            Ok(AskOutcome::Answered(answer))
        }
        Ok(None) => Ok(AskOutcome::TimedOut),
        Err(error) => Err(IronCrewError::Validation(format!(
            "controlling terminal read failed: {error}"
        ))),
    }
}

/// Duplicate the verified-TTY stdin descriptor and poll it alongside a
/// cancellation socket. At the deadline the async side signals cancellation
/// and joins the bounded blocking reader before returning. There is therefore
/// no stale reader left to steal an answer from a later question or hold
/// runtime shutdown open. The duplicate is not made nonblocking because file
/// status flags are shared with stdin; canonical TTY readiness makes the
/// subsequent single-byte read safe.
#[cfg(unix)]
async fn read_tty_line(
    max_bytes: usize,
    timeout: std::time::Duration,
) -> std::io::Result<Option<String>> {
    use std::os::fd::AsFd;

    let tty = nix::unistd::dup(std::io::stdin().as_fd())
        .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
    nix::fcntl::fcntl(
        &tty,
        nix::fcntl::FcntlArg::F_SETFD(nix::fcntl::FdFlag::FD_CLOEXEC),
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error as i32))?;
    let tty = std::fs::File::from(tty);
    let (cancel_rx, mut cancel_tx) = std::os::unix::net::UnixStream::pair()?;
    let mut reader =
        tokio::task::spawn_blocking(move || read_tty_line_blocking(tty, cancel_rx, max_bytes));

    tokio::select! {
        biased;
        result = &mut reader => join_tty_reader(result),
        _ = tokio::time::sleep(timeout) => {
            // A byte makes the cancellation descriptor readable. Dropping the
            // sender is a second wake-up path if the write itself fails.
            let _ = cancel_tx.write_all(&[1]);
            drop(cancel_tx);
            let _ = join_tty_reader(reader.await)?;
            Ok(None)
        }
    }
}

#[cfg(unix)]
fn join_tty_reader(
    result: std::result::Result<std::io::Result<Option<String>>, tokio::task::JoinError>,
) -> std::io::Result<Option<String>> {
    result
        .map_err(|error| std::io::Error::other(format!("terminal reader task failed: {error}")))?
}

#[cfg(unix)]
fn read_tty_line_blocking(
    mut tty: std::fs::File,
    cancel: std::os::unix::net::UnixStream,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    use std::io::Read as _;
    use std::os::fd::AsFd;

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};

    let mut bytes = Vec::new();
    let mut oversized = false;
    let cancel_flags =
        PollFlags::POLLIN | PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL;
    let tty_failures = PollFlags::POLLERR | PollFlags::POLLNVAL;

    loop {
        let mut descriptors = [
            PollFd::new(tty.as_fd(), PollFlags::POLLIN),
            PollFd::new(cancel.as_fd(), PollFlags::POLLIN),
        ];
        match poll(&mut descriptors, PollTimeout::NONE) {
            Ok(_) => {}
            Err(nix::errno::Errno::EINTR) => continue,
            Err(error) => return Err(std::io::Error::from_raw_os_error(error as i32)),
        }

        // Cancellation wins if input and the deadline arrive together. Once a
        // line is already known to be oversized, discard the terminal input
        // queue before returning the size error: `is_terminal()` does not
        // guarantee canonical mode, so waiting for a newline could otherwise
        // violate the deadline and spin on the permanently-readable cancel FD.
        if descriptors[1]
            .revents()
            .unwrap_or_else(PollFlags::empty)
            .intersects(cancel_flags)
        {
            if oversized {
                let _ =
                    nix::sys::termios::tcflush(tty.as_fd(), nix::sys::termios::FlushArg::TCIFLUSH);
                return Err(oversized_tty_answer_error(max_bytes));
            }
            return Ok(None);
        }

        let tty_events = descriptors[0].revents().unwrap_or_else(PollFlags::empty);
        if tty_events.intersects(tty_failures) {
            return Err(std::io::Error::other(format!(
                "terminal poll failed with {tty_events:?}"
            )));
        }
        if tty_events.intersects(PollFlags::POLLIN | PollFlags::POLLHUP) {
            let mut byte = [0_u8; 1];
            match tty.read(&mut byte) {
                Ok(0) if oversized => return Err(oversized_tty_answer_error(max_bytes)),
                Ok(0) => return Ok(None),
                Ok(_) if byte[0] == b'\n' => {
                    if oversized {
                        return Err(oversized_tty_answer_error(max_bytes));
                    }
                    return String::from_utf8(bytes).map(Some).map_err(|error| {
                        std::io::Error::new(std::io::ErrorKind::InvalidData, error)
                    });
                }
                Ok(_) => {
                    if oversized {
                        continue;
                    }
                    if bytes.len() >= max_bytes {
                        oversized = true;
                        bytes.clear();
                        continue;
                    }
                    bytes.push(byte[0]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(unix)]
fn oversized_tty_answer_error(max_bytes: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("terminal answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES ({max_bytes})"),
    )
}

#[cfg(windows)]
async fn read_tty_line(
    max_bytes: usize,
    timeout: std::time::Duration,
) -> std::io::Result<Option<String>> {
    use crossterm::event::{Event, EventStream, KeyCode, KeyEventKind};
    use futures::StreamExt;

    let read = async {
        let mut events = EventStream::new();
        let mut line = String::new();
        while let Some(event) = events.next().await {
            let Event::Key(key) = event? else {
                continue;
            };
            if !matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) {
                continue;
            }
            match key.code {
                KeyCode::Enter => {
                    eprintln!();
                    return Ok(Some(line));
                }
                KeyCode::Backspace => {
                    if line.pop().is_some() {
                        eprint!("\u{8} \u{8}");
                        std::io::stderr().flush()?;
                    }
                }
                KeyCode::Char(character) => {
                    if line.len().saturating_add(character.len_utf8()) > max_bytes {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            format!(
                                "terminal answer exceeds IRONCREW_ASK_HUMAN_MAX_ANSWER_BYTES ({max_bytes})"
                            ),
                        ));
                    }
                    line.push(character);
                    eprint!("{character}");
                    std::io::stderr().flush()?;
                }
                _ => {}
            }
        }
        Ok(None)
    };

    match tokio::time::timeout(timeout, read).await {
        Ok(result) => result,
        Err(_) => Ok(None),
    }
}

#[cfg(not(any(unix, windows)))]
async fn read_tty_line(
    _max_bytes: usize,
    _timeout: std::time::Duration,
) -> std::io::Result<Option<String>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "interactive terminal input is unsupported on this platform",
    ))
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
    async fn delivered_answer_wins_when_timeout_is_already_ready() {
        let (tx, rx) = oneshot::channel();
        tx.send(json!("at-the-deadline")).unwrap();

        match await_http_answer(rx, std::time::Duration::ZERO).await {
            AskOutcome::Answered(value) => assert_eq!(value, json!("at-the-deadline")),
            other => panic!("expected delivered answer to win, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn sibling_start_queues_waiting_after_inflight_running_write() {
        let bridge = std::sync::Arc::new(bridge());
        let completed_wait = bridge.begin_run_status_wait(None);
        completed_wait.disarm();

        let statuses = std::sync::Arc::new(Mutex::new(Vec::new()));
        let running_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_running = std::sync::Arc::new(tokio::sync::Notify::new());
        let first = {
            let bridge = bridge.clone();
            let statuses = statuses.clone();
            let running_started = running_started.clone();
            let release_running = release_running.clone();
            tokio::spawn(async move {
                bridge
                    .sync_run_status_with(|status| async move {
                        statuses.lock().unwrap().push(status);
                        running_started.notify_one();
                        release_running.notified().await;
                        Ok(())
                    })
                    .await
            })
        };

        running_started.notified().await;
        let sibling_wait = bridge.begin_run_status_wait(None);
        let second = {
            let bridge = bridge.clone();
            let statuses = statuses.clone();
            tokio::spawn(async move {
                bridge
                    .sync_run_status_with(|status| async move {
                        statuses.lock().unwrap().push(status);
                        Ok(())
                    })
                    .await
            })
        };

        release_running.notify_one();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(
            *statuses.lock().unwrap(),
            vec![
                crate::engine::run_history::RunStatus::Running,
                crate::engine::run_history::RunStatus::WaitingForInput,
            ]
        );
        sibling_wait.disarm();
    }

    #[tokio::test]
    async fn sibling_finish_queues_running_after_inflight_waiting_write() {
        let bridge = std::sync::Arc::new(bridge());
        let sibling_wait = bridge.begin_run_status_wait(None);

        let statuses = std::sync::Arc::new(Mutex::new(Vec::new()));
        let waiting_started = std::sync::Arc::new(tokio::sync::Notify::new());
        let release_waiting = std::sync::Arc::new(tokio::sync::Notify::new());
        let first = {
            let bridge = bridge.clone();
            let statuses = statuses.clone();
            let waiting_started = waiting_started.clone();
            let release_waiting = release_waiting.clone();
            tokio::spawn(async move {
                bridge
                    .sync_run_status_with(|status| async move {
                        statuses.lock().unwrap().push(status);
                        waiting_started.notify_one();
                        release_waiting.notified().await;
                        Ok(())
                    })
                    .await
            })
        };

        waiting_started.notified().await;
        sibling_wait.disarm();
        let second = {
            let bridge = bridge.clone();
            let statuses = statuses.clone();
            tokio::spawn(async move {
                bridge
                    .sync_run_status_with(|status| async move {
                        statuses.lock().unwrap().push(status);
                        Ok(())
                    })
                    .await
            })
        };

        release_waiting.notify_one();
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(
            *statuses.lock().unwrap(),
            vec![
                crate::engine::run_history::RunStatus::WaitingForInput,
                crate::engine::run_history::RunStatus::Running,
            ]
        );
    }

    #[tokio::test]
    async fn cancelling_a_branch_removes_its_question_and_restores_running_status() {
        use crate::engine::run_history::{JsonFileStore, RunIntent, RunStatus};
        use crate::engine::store::{RunLeaseConfig, StateStore};

        let temp = tempfile::tempdir().unwrap();
        let lease = RunLeaseConfig::new(
            format!("input-bridge-cancel-test-{}", uuid::Uuid::new_v4()),
            std::time::Duration::from_secs(60),
        )
        .unwrap();
        let store: std::sync::Arc<dyn StateStore> = std::sync::Arc::new(
            JsonFileStore::new_with_lease_config(temp.path().join(".ironcrew"), lease).unwrap(),
        );
        let run_id = store
            .save_run_intent(RunIntent {
                suggested_id: Some(uuid::Uuid::new_v4().to_string()),
                flow_name: "cancellation cleanup".into(),
                flow: "test".into(),
                started_at: chrono::Utc::now().to_rfc3339(),
                agent_count: 0,
                task_count: 0,
                tags: Vec::new(),
            })
            .await
            .unwrap();

        let bridge = std::sync::Arc::new(bridge());
        let branch = {
            let bridge = bridge.clone();
            let store = store.clone();
            let run_id = run_id.clone();
            tokio::spawn(async move {
                bridge
                    .with_run_wait_status(
                        Some(store),
                        Some(&run_id),
                        bridge.ask("cancelled", "Wait", &[], 30, "question"),
                    )
                    .await
            })
        };

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if bridge.pending_count() == 1
                    && store.get_run(&run_id).await.unwrap().status == RunStatus::WaitingForInput
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("question never reached its durable waiting state");

        branch.abort();
        assert!(branch.await.unwrap_err().is_cancelled());
        assert_eq!(bridge.pending_count(), 0);
        assert!(bridge.answer("cancelled", json!("late")).is_err());

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if store.get_run(&run_id).await.unwrap().status == RunStatus::Running {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled branch left the durable run waiting for input");
    }

    #[tokio::test]
    async fn tty_asks_wait_for_exclusive_prompt_ownership() {
        let b = std::sync::Arc::new(InputBridge::new(BridgeMode::Tty));
        let prompt_guard = b.tty_prompt.lock().await;
        let b2 = b.clone();
        let mut queued =
            tokio::spawn(async move { b2.ask("q1", "queued prompt", &[], 30, "question").await });

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), &mut queued)
                .await
                .is_err(),
            "a second TTY ask bypassed exclusive prompt ownership"
        );

        queued.abort();
        drop(prompt_guard);
        assert!(queued.await.unwrap_err().is_cancelled());
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
        assert_eq!(
            b.answer("nope", json!(1)),
            Err(AnswerError::UnknownOrExpired {
                question_id: "nope".to_string(),
            })
        );
    }

    #[test]
    fn oversized_http_answer_has_a_typed_error() {
        let b = bridge();
        let oversized = "x".repeat(HARD_MAX_ANSWER_BYTES + 1);
        assert!(matches!(
            b.answer("nope", json!(oversized)),
            Err(AnswerError::TooLarge { .. })
        ));
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
    async fn expire_all_closes_transport_and_wakes_pending_asks() {
        let b = std::sync::Arc::new(bridge());
        let b2 = b.clone();
        let ask = tokio::spawn(async move { b2.ask("q1", "Wait", &[], 30, "question").await });
        while b.pending_count() == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

        assert_eq!(b.expire_all(), 1);
        assert!(b.is_expired());
        assert!(b.list().is_empty());
        assert!(b.answer("q1", json!("late")).is_err());
        assert!(b.ask("q2", "Too late", &[], 30, "question").await.is_err());
        assert!(matches!(ask.await.unwrap().unwrap(), AskOutcome::TimedOut));
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
