use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

#[derive(Debug, Default)]
struct EventBusMetricTotals {
    instances: AtomicU64,
    retained_events: AtomicU64,
    retained_bytes: AtomicU64,
    event_capacity: AtomicU64,
    byte_capacity: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EventBusMetricSnapshot {
    pub instances: u64,
    pub retained_events: u64,
    pub retained_bytes: u64,
    pub event_capacity: u64,
    pub byte_capacity: u64,
}

static GLOBAL_TOTALS: OnceLock<Arc<EventBusMetricTotals>> = OnceLock::new();

fn global_totals() -> Arc<EventBusMetricTotals> {
    Arc::clone(GLOBAL_TOTALS.get_or_init(|| Arc::new(EventBusMetricTotals::default())))
}

pub(crate) fn eventbus_metric_snapshot() -> EventBusMetricSnapshot {
    global_totals().snapshot()
}

pub(super) struct EventBusMetricRegistration {
    totals: Arc<EventBusMetricTotals>,
    retained_events: AtomicU64,
    retained_bytes: AtomicU64,
    event_capacity: u64,
    byte_capacity: u64,
}

impl EventBusMetricRegistration {
    pub(super) fn new(event_capacity: usize, byte_capacity: usize) -> Self {
        Self::new_with_totals(event_capacity, byte_capacity, global_totals())
    }

    fn new_with_totals(
        event_capacity: usize,
        byte_capacity: usize,
        totals: Arc<EventBusMetricTotals>,
    ) -> Self {
        let event_capacity = u64::try_from(event_capacity).unwrap_or(u64::MAX);
        let byte_capacity = u64::try_from(byte_capacity).unwrap_or(u64::MAX);
        totals.instances.fetch_add(1, Ordering::AcqRel);
        totals
            .event_capacity
            .fetch_add(event_capacity, Ordering::AcqRel);
        totals
            .byte_capacity
            .fetch_add(byte_capacity, Ordering::AcqRel);
        Self {
            totals,
            retained_events: AtomicU64::new(0),
            retained_bytes: AtomicU64::new(0),
            event_capacity,
            byte_capacity,
        }
    }

    pub(super) fn set_retained(&self, events: usize, bytes: usize) {
        let events = u64::try_from(events).unwrap_or(u64::MAX);
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        adjust(
            &self.totals.retained_events,
            self.retained_events.swap(events, Ordering::AcqRel),
            events,
        );
        adjust(
            &self.totals.retained_bytes,
            self.retained_bytes.swap(bytes, Ordering::AcqRel),
            bytes,
        );
    }
}

impl Drop for EventBusMetricRegistration {
    fn drop(&mut self) {
        self.totals.instances.fetch_sub(1, Ordering::AcqRel);
        self.totals.retained_events.fetch_sub(
            self.retained_events.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        self.totals.retained_bytes.fetch_sub(
            self.retained_bytes.load(Ordering::Acquire),
            Ordering::AcqRel,
        );
        self.totals
            .event_capacity
            .fetch_sub(self.event_capacity, Ordering::AcqRel);
        self.totals
            .byte_capacity
            .fetch_sub(self.byte_capacity, Ordering::AcqRel);
    }
}

impl EventBusMetricTotals {
    fn snapshot(&self) -> EventBusMetricSnapshot {
        EventBusMetricSnapshot {
            instances: self.instances.load(Ordering::Acquire),
            retained_events: self.retained_events.load(Ordering::Acquire),
            retained_bytes: self.retained_bytes.load(Ordering::Acquire),
            event_capacity: self.event_capacity.load(Ordering::Acquire),
            byte_capacity: self.byte_capacity.load(Ordering::Acquire),
        }
    }
}

fn adjust(total: &AtomicU64, old: u64, new: u64) {
    if new >= old {
        total.fetch_add(new - old, Ordering::AcqRel);
    } else {
        total.fetch_sub(old - new, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_updates_totals_and_deregisters_without_scanning() {
        let totals = Arc::new(EventBusMetricTotals::default());
        {
            let registration =
                EventBusMetricRegistration::new_with_totals(10, 1_000, Arc::clone(&totals));
            registration.set_retained(3, 300);
            assert_eq!(
                totals.snapshot(),
                EventBusMetricSnapshot {
                    instances: 1,
                    retained_events: 3,
                    retained_bytes: 300,
                    event_capacity: 10,
                    byte_capacity: 1_000,
                }
            );
            registration.set_retained(1, 80);
            assert_eq!(totals.snapshot().retained_events, 1);
            assert_eq!(totals.snapshot().retained_bytes, 80);
        }
        assert_eq!(
            totals.snapshot(),
            EventBusMetricSnapshot {
                instances: 0,
                retained_events: 0,
                retained_bytes: 0,
                event_capacity: 0,
                byte_capacity: 0,
            }
        );
    }
}
