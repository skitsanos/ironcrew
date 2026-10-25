//! Read-only historical projections, deliberately separate from executable run records.
//!
//! Missing receipts remain `None`; legacy counters are retained only in the original record.
//! No conversion to `RunRecord` or execution state is provided.

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, value::RawValue};

use crate::usage::UsageSnapshot;
use crate::utils::error::{IronCrewError, Result};

use crate::engine::run_history::RunStatus;

#[cfg(feature = "postgres")]
#[path = "history_reader/postgres.rs"]
pub mod postgres;
#[cfg(feature = "postgres")]
pub use postgres::PostgresHistoryReader;

pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
const MAX_TASKS: usize = 10_000;

/// Inspection-only output. `record` retains the row's exact JSON, including numeric precision.
#[derive(Debug, Serialize)]
pub struct HistoryRecord {
    pub read_only: bool,
    pub executable: bool,
    pub legacy_usage_missing: bool,
    pub run_usage: Option<UsageSnapshot>,
    pub tasks: Vec<HistoryTaskUsage>,
    pub record: Box<RawValue>,
}

#[derive(Debug, Serialize)]
pub struct HistoryTaskUsage {
    pub index: usize,
    pub usage: Option<UsageSnapshot>,
    pub legacy_counters_present: bool,
}

// Absence is unknown, while an explicitly present JSON null must fail checked receipt decoding.
#[derive(Default)]
struct StoredJson(Option<Box<RawValue>>);

impl<'de> Deserialize<'de> for StoredJson {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        Box::<RawValue>::deserialize(deserializer).map(|raw| Self(Some(raw)))
    }
}

// Ignored historical fields are skipped without converting their numbers through f64.
#[derive(Deserialize)]
struct Header {
    run_id: String,
    flow_name: String,
    #[serde(default)]
    flow: String,
    status: String,
    started_at: String,
    finished_at: String,
    duration_ms: u64,
    agent_count: usize,
    task_count: usize,
    #[serde(default)]
    usage: StoredJson,
    #[serde(deserialize_with = "bounded_tasks")]
    task_results: Vec<TaskHeader>,
}

#[derive(Deserialize)]
struct TaskHeader {
    task: String,
    agent: String,
    output: String,
    success: bool,
    duration_ms: u64,
    #[serde(default)]
    reasoning: Option<String>,
    #[serde(default)]
    usage: StoredJson,
    #[serde(default)]
    token_usage: StoredJson,
}

fn bounded_tasks<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Vec<TaskHeader>, D::Error> {
    struct Tasks;
    impl<'de> serde::de::Visitor<'de> for Tasks {
        type Value = Vec<TaskHeader>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("at most 10000 historical task results")
        }
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut tasks = Vec::new();
            while let Some(task) = seq.next_element()? {
                if tasks.len() == MAX_TASKS {
                    return Err(serde::de::Error::custom(
                        "historical task count exceeds 10000",
                    ));
                }
                tasks.push(task);
            }
            Ok(tasks)
        }
    }
    deserializer.deserialize_seq(Tasks)
}

pub(super) fn invalid(message: &str) -> IronCrewError {
    IronCrewError::Validation(message.into())
}

fn checked_usage(value: StoredJson) -> Result<Option<UsageSnapshot>> {
    value
        .0
        .map(|raw| {
            if raw.get().len() > 4096 {
                return Err(invalid("Historical checked usage exceeds 4096 bytes"));
            }
            serde_json::from_str(raw.get())
                .map_err(|_| invalid("Historical checked usage is invalid"))
        })
        .transpose()
}

/// Decode an inspection view without writing, repairing or inventing usage.
pub fn decode_history_record(raw: &str) -> Result<HistoryRecord> {
    if raw.len() > MAX_RECORD_BYTES {
        return Err(invalid("Historical record exceeds 8 MiB"));
    }
    let Header {
        run_id,
        flow_name,
        flow,
        status,
        started_at,
        finished_at,
        duration_ms,
        agent_count,
        task_count,
        usage,
        task_results,
    } = serde_json::from_str(raw).map_err(|_| invalid("Historical record shape is invalid"))?;
    // SQL stores lowercase status strings; public JSON uses enum names. Preserve either spelling.
    status.parse::<RunStatus>().or_else(|_| {
        serde_json::from_value(Value::String(status.clone()))
            .map_err(|_| invalid("Historical run status is invalid"))
    })?;
    drop((run_id, flow_name, flow, status, started_at, finished_at));
    let _ = (duration_ms, agent_count, task_count);
    let run_usage = checked_usage(usage)?;
    let mut tasks = Vec::with_capacity(task_results.len());
    for (index, task) in task_results.into_iter().enumerate() {
        let TaskHeader {
            task: name,
            agent,
            output,
            success,
            duration_ms,
            reasoning,
            usage,
            token_usage,
        } = task;
        drop((name, agent, output, reasoning));
        let _ = (success, duration_ms);
        tasks.push(HistoryTaskUsage {
            index,
            usage: checked_usage(usage)?,
            legacy_counters_present: token_usage.0.is_some(),
        });
    }
    let record = RawValue::from_string(raw.to_owned())
        .map_err(|_| invalid("Historical record is not valid JSON"))?;
    Ok(HistoryRecord {
        read_only: true,
        executable: false,
        legacy_usage_missing: run_usage.is_none() || tasks.iter().any(|task| task.usage.is_none()),
        run_usage,
        tasks,
        record,
    })
}
