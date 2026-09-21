use super::*;

#[cfg(test)]
#[path = "tests.rs"]
mod tests;

const DEFAULT_TASK_RESULT_MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
const HARD_TASK_RESULT_MAX_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_TASK_RESULT_MAX_REASONING_BYTES: usize = 4 * 1024 * 1024;
const HARD_TASK_RESULT_MAX_REASONING_BYTES: usize = 16 * 1024 * 1024;
const DEFAULT_RUN_RESULTS_MAX_BYTES: usize = 32 * 1024 * 1024;
// The JSON store defaults to 64 MiB per record. Keep at least 16 MiB for the
// RunRecord envelope, tags, goal, and JSON escaping/metadata overhead.
const HARD_RUN_RESULTS_MAX_BYTES: usize = 48 * 1024 * 1024;

fn configured_byte_limit(name: &str, default: usize, hard_max: usize) -> Result<usize> {
    match std::env::var(name) {
        Ok(raw) => {
            let value = raw.parse::<usize>().map_err(|_| {
                IronCrewError::Validation(format!(
                    "{name} must be an integer between 1 and {hard_max}"
                ))
            })?;
            if value == 0 || value > hard_max {
                return Err(IronCrewError::Validation(format!(
                    "{name} must be between 1 and {hard_max}; got {value}"
                )));
            }
            Ok(value)
        }
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(std::env::VarError::NotUnicode(_)) => Err(IronCrewError::Validation(format!(
            "{name} must contain valid UTF-8"
        ))),
    }
}

#[derive(Debug)]
struct CountingWriter {
    bytes: usize,
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(buffer.len())
            .ok_or_else(|| std::io::Error::other("serialized TaskResult size overflow"))?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialized_result_bytes(result: &TaskResult) -> Result<usize> {
    let mut writer = CountingWriter { bytes: 0 };
    serde_json::to_writer(&mut writer, result).map_err(|error| {
        IronCrewError::Validation(format!(
            "Failed to size task result '{}': {error}",
            result.task
        ))
    })?;
    Ok(writer.bytes)
}

/// Tracks the serialized bytes retained in the run result map. This is the
/// representation ultimately persisted, so the aggregate ceiling protects
/// both process RSS and the 64 MiB JSON-store record budget.
pub(super) struct RetainedResultBudget {
    max_output_bytes: usize,
    max_reasoning_bytes: usize,
    max_total_bytes: usize,
    total_bytes: usize,
}

impl RetainedResultBudget {
    pub(super) fn from_env() -> Result<Self> {
        Ok(Self {
            max_output_bytes: configured_byte_limit(
                "IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES",
                DEFAULT_TASK_RESULT_MAX_OUTPUT_BYTES,
                HARD_TASK_RESULT_MAX_OUTPUT_BYTES,
            )?,
            max_reasoning_bytes: configured_byte_limit(
                "IRONCREW_TASK_RESULT_MAX_REASONING_BYTES",
                DEFAULT_TASK_RESULT_MAX_REASONING_BYTES,
                HARD_TASK_RESULT_MAX_REASONING_BYTES,
            )?,
            max_total_bytes: configured_byte_limit(
                "IRONCREW_RUN_RESULTS_MAX_BYTES",
                DEFAULT_RUN_RESULTS_MAX_BYTES,
                HARD_RUN_RESULTS_MAX_BYTES,
            )?,
            total_bytes: 0,
        })
    }

    pub(super) fn insert(
        &mut self,
        results: &mut HashMap<String, TaskResult>,
        key: String,
        result: TaskResult,
    ) -> Result<()> {
        if result.output.len() > self.max_output_bytes {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' output is {} bytes, exceeds IRONCREW_TASK_RESULT_MAX_OUTPUT_BYTES ({})",
                result.task,
                result.output.len(),
                self.max_output_bytes
            )));
        }
        if let Some(reasoning) = result.reasoning.as_ref()
            && reasoning.len() > self.max_reasoning_bytes
        {
            return Err(IronCrewError::Validation(format!(
                "Task '{}' reasoning is {} bytes, exceeds IRONCREW_TASK_RESULT_MAX_REASONING_BYTES ({})",
                result.task,
                reasoning.len(),
                self.max_reasoning_bytes
            )));
        }

        let result_bytes = serialized_result_bytes(&result)?;
        let replaced_bytes = results
            .get(&key)
            .map(serialized_result_bytes)
            .transpose()?
            .unwrap_or(0);
        let new_total = self
            .total_bytes
            .saturating_sub(replaced_bytes)
            .checked_add(result_bytes)
            .ok_or_else(|| IronCrewError::Validation("Run result byte count overflowed".into()))?;
        if new_total > self.max_total_bytes {
            return Err(IronCrewError::Validation(format!(
                "Retaining task '{}' would grow serialized run results to {} bytes, exceeding IRONCREW_RUN_RESULTS_MAX_BYTES ({})",
                result.task, new_total, self.max_total_bytes
            )));
        }

        results.insert(key, result);
        self.total_bytes = new_total;
        Ok(())
    }
}
