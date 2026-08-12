use std::fmt::Write as _;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use super::AppState;

const PROCESS_MEMORY_CACHE_TTL: Duration = Duration::from_secs(1);
#[cfg(target_os = "linux")]
const PROC_STATUS_MAX_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProcessMemorySample {
    resident_bytes: u64,
    peak_resident_bytes: u64,
}

#[derive(Debug, Clone, Copy)]
struct CachedProcessMemory {
    checked_at: Instant,
    sample: Option<ProcessMemorySample>,
}

static PROCESS_MEMORY_CACHE: OnceLock<tokio::sync::Mutex<Option<CachedProcessMemory>>> =
    OnceLock::new();

pub(super) async fn append(body: &mut String, state: &AppState) {
    let provider = crate::llm::metrics::provider_call_snapshot();
    write_helped_gauge(
        body,
        "ironcrew_process_active_provider_calls",
        "Logical LLM provider futures currently active in this process, including provider pacing waits.",
        provider.active,
    );
    write_helped_gauge(
        body,
        "ironcrew_process_peak_active_provider_calls",
        "Peak logical LLM provider futures active concurrently in this process since startup.",
        provider.peak,
    );

    let eventbus = crate::engine::eventbus_metrics::eventbus_metric_snapshot();
    for (name, help, value) in [
        (
            "ironcrew_process_eventbus_instances",
            "EventBus replay buffers currently registered in this process.",
            eventbus.instances,
        ),
        (
            "ironcrew_process_eventbus_retained_events",
            "Events currently retained across process-local EventBus replay buffers.",
            eventbus.retained_events,
        ),
        (
            "ironcrew_process_eventbus_retained_bytes",
            "Approximate serialized bytes retained across process-local EventBus replay buffers; this is not allocator or RSS usage.",
            eventbus.retained_bytes,
        ),
        (
            "ironcrew_process_eventbus_retained_events_capacity",
            "Sum of configured event-count capacities across registered process-local EventBus replay buffers.",
            eventbus.event_capacity,
        ),
        (
            "ironcrew_process_eventbus_retained_bytes_capacity",
            "Sum of configured byte capacities across registered process-local EventBus replay buffers.",
            eventbus.byte_capacity,
        ),
    ] {
        write_helped_gauge(body, name, help, value);
    }

    if let Some(pool) = state.store.postgres_pool_usage() {
        write_helped_gauge(
            body,
            "ironcrew_postgres_pool_open_connections",
            "SQLx PostgreSQL connections currently open in this process pool.",
            pool.open_connections,
        );
        write_helped_gauge(
            body,
            "ironcrew_postgres_pool_in_use_connections",
            "SQLx PostgreSQL connections currently checked out from this process pool.",
            pool.in_use_connections,
        );
        write_helped_gauge(
            body,
            "ironcrew_postgres_pool_connections_limit",
            "Configured maximum connections for this process SQLx PostgreSQL pool.",
            pool.connection_limit,
        );
    }

    let memory = process_memory_sample().await;
    write_helped_gauge(
        body,
        "ironcrew_process_memory_measurement_available",
        "Linux /proc/self memory measurement availability; excludes cgroup limits, OOM events, and child processes.",
        u8::from(memory.is_some()),
    );
    if let Some(memory) = memory {
        write_helped_gauge(
            body,
            "ironcrew_process_resident_memory_bytes",
            "Linux /proc/self/status VmRSS for this process only; excludes cgroup and child-process memory.",
            memory.resident_bytes,
        );
        write_helped_gauge(
            body,
            "ironcrew_process_peak_resident_memory_bytes",
            "Linux /proc/self/status VmHWM for this process only; excludes cgroup and child-process memory.",
            memory.peak_resident_bytes,
        );
    }
}

fn write_helped_gauge<T: std::fmt::Display>(body: &mut String, name: &str, help: &str, value: T) {
    writeln!(body, "# HELP {name} {help}").unwrap();
    writeln!(body, "# TYPE {name} gauge").unwrap();
    writeln!(body, "{name} {value}").unwrap();
}

async fn process_memory_sample() -> Option<ProcessMemorySample> {
    let cache = PROCESS_MEMORY_CACHE.get_or_init(|| tokio::sync::Mutex::new(None));
    let mut cached = cache.lock().await;
    if let Some(snapshot) = *cached
        && snapshot.checked_at.elapsed() < PROCESS_MEMORY_CACHE_TTL
    {
        return snapshot.sample;
    }
    let sample = read_process_memory().await;
    *cached = Some(CachedProcessMemory {
        checked_at: Instant::now(),
        sample,
    });
    sample
}

#[cfg(target_os = "linux")]
async fn read_process_memory() -> Option<ProcessMemorySample> {
    use tokio::io::AsyncReadExt as _;

    let file = tokio::fs::File::open("/proc/self/status").await.ok()?;
    let mut bytes = Vec::with_capacity(PROC_STATUS_MAX_BYTES as usize + 1);
    file.take(PROC_STATUS_MAX_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() > PROC_STATUS_MAX_BYTES as usize {
        return None;
    }
    parse_proc_status(std::str::from_utf8(&bytes).ok()?)
}

#[cfg(not(target_os = "linux"))]
async fn read_process_memory() -> Option<ProcessMemorySample> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_proc_status(status: &str) -> Option<ProcessMemorySample> {
    fn kibibytes(line: &str, prefix: &str) -> Option<u64> {
        let mut fields = line.strip_prefix(prefix)?.split_ascii_whitespace();
        let value = fields.next()?.parse::<u64>().ok()?;
        if fields.next()? != "kB" || fields.next().is_some() {
            return None;
        }
        value.checked_mul(1024)
    }

    let mut resident_bytes = None;
    let mut peak_resident_bytes = None;
    for line in status.lines() {
        if line.starts_with("VmRSS:") {
            resident_bytes = Some(kibibytes(line, "VmRSS:")?);
        } else if line.starts_with("VmHWM:") {
            peak_resident_bytes = Some(kibibytes(line, "VmHWM:")?);
        }
    }
    Some(ProcessMemorySample {
        resident_bytes: resident_bytes?,
        peak_resident_bytes: peak_resident_bytes?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_status_parser_requires_both_bounded_kibibyte_fields() {
        assert_eq!(
            parse_proc_status("Name:\tironcrew\nVmHWM:\t42 kB\nVmRSS:\t21 kB\n"),
            Some(ProcessMemorySample {
                resident_bytes: 21 * 1024,
                peak_resident_bytes: 42 * 1024,
            })
        );
        assert!(parse_proc_status("VmRSS: 21 kB\n").is_none());
        assert!(parse_proc_status("VmRSS: 21 MB\nVmHWM: 42 kB\n").is_none());
        assert!(parse_proc_status("VmRSS: nope kB\nVmHWM: 42 kB\n").is_none());
        assert!(parse_proc_status(&format!("VmRSS: {} kB\nVmHWM: 42 kB\n", u64::MAX)).is_none());
    }
}
