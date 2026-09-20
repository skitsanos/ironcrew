pub(super) const DEFAULT_EVENT_MAX_BYTES: usize = 256 * 1024;
pub(super) const HARD_EVENT_MAX_BYTES: usize = 16 * 1024 * 1024;
pub(super) const DEFAULT_REPLAY_MAX_EVENTS: usize = 1_000;
pub(super) const HARD_REPLAY_MAX_EVENTS: usize = 10_000;
pub(super) const DEFAULT_REPLAY_MAX_BYTES: usize = 4 * 1024 * 1024;
pub(super) const HARD_REPLAY_MAX_BYTES: usize = 64 * 1024 * 1024;
pub(super) const DEFAULT_LIVE_CHANNEL_CAPACITY: usize = 32;
pub(super) const HARD_LIVE_CHANNEL_CAPACITY: usize = 256;
pub(super) const DEFAULT_DURABLE_QUEUE_MAX_EVENTS: usize = 64;
pub(super) const DEFAULT_DURABLE_QUEUE_MAX_BYTES: usize = 1024 * 1024;
pub(super) const DEFAULT_DURABLE_BATCH_MAX_EVENTS: usize = 32;
pub(super) const TRUNCATION_MARKER: &str = "... [truncated]";

pub(super) fn bounded_env(name: &str, default: usize, min: usize, max: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value >= min)
        .map(|value| value.min(max))
        .unwrap_or(default)
}
