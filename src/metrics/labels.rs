macro_rules! fixed_labels {
    ($name:ident { $($variant:ident => $label:literal),+ $(,)? }) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(usize)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            pub(crate) const ALL: &'static [Self] = &[$(Self::$variant),+];
            pub(crate) const COUNT: usize = Self::ALL.len();

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $label),+
                }
            }

            pub(crate) const fn index(self) -> usize {
                self as usize
            }
        }
    };
}

fixed_labels!(RunOutcome {
    Success => "success",
    PartialFailure => "partial_failure",
    Failed => "failed",
    Aborted => "aborted",
    TimedOut => "timed_out",
    Abandoned => "abandoned",
});

impl RunOutcome {
    pub fn from_status(status: &crate::engine::run_history::RunStatus) -> Option<Self> {
        use crate::engine::run_history::RunStatus;

        match status {
            RunStatus::Success => Some(Self::Success),
            RunStatus::PartialFailure => Some(Self::PartialFailure),
            RunStatus::Failed => Some(Self::Failed),
            RunStatus::Aborted => Some(Self::Aborted),
            RunStatus::TimedOut => Some(Self::TimedOut),
            RunStatus::Abandoned => Some(Self::Abandoned),
            RunStatus::Running | RunStatus::WaitingForInput => None,
        }
    }
}

fixed_labels!(TaskOutcome {
    Success => "success",
    Error => "error",
    Skipped => "skipped",
    Cancelled => "cancelled",
});

fixed_labels!(ToolOutcome {
    Success => "success",
    Error => "error",
    Cancelled => "cancelled",
});

fixed_labels!(ProviderFamily {
    OpenAi => "openai",
    OpenAiResponses => "openai_responses",
    Anthropic => "anthropic",
    Other => "other",
});

fixed_labels!(ProviderOperation {
    Chat => "chat",
    ChatWithTools => "chat_with_tools",
    ChatStream => "chat_stream",
});

fixed_labels!(ProviderOutcome {
    Success => "success",
    Error => "error",
    Cancelled => "cancelled",
});

fixed_labels!(TokenKind {
    Prompt => "prompt",
    Completion => "completion",
    Cached => "cached",
});

fixed_labels!(SseScope {
    RunProcess => "run_process",
    RunShared => "run_shared",
    ConversationProcess => "conversation_process",
});

fixed_labels!(SseOutcome {
    Accepted => "accepted",
    Limited => "limited",
});

fixed_labels!(LeaseScope {
    Run => "run",
    Conversation => "conversation",
});

fixed_labels!(ReconciliationOutcome {
    Success => "success",
    Error => "error",
});

fixed_labels!(TerminalScope {
    RunRecord => "run_record",
    RunIdempotency => "run_idempotency",
    RunIndeterminate => "run_indeterminate",
    ConversationCommit => "conversation_commit",
    ConversationIndeterminate => "conversation_indeterminate",
});

fixed_labels!(TerminalOutcome {
    Success => "success",
    Error => "error",
    Fenced => "fenced",
});

impl TerminalOutcome {
    pub const fn from_applied(applied: bool) -> Self {
        if applied { Self::Success } else { Self::Fenced }
    }
}

fixed_labels!(StoreOperation {
    MetricsSnapshot => "metrics_snapshot",
    Readiness => "readiness",
    MaintenanceHeartbeat => "maintenance_heartbeat",
    Reconciliation => "reconciliation",
    LeaseHeartbeat => "lease_heartbeat",
    TerminalPersistence => "terminal_persistence",
    EventAppend => "event_append",
    EventRead => "event_read",
    Audit => "audit",
    Run => "run",
    Idempotency => "idempotency",
    Conversation => "conversation",
    HumanInput => "human_input",
});
