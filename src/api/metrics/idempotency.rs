//! Store-backed idempotency utilization series.

use std::fmt::Write as _;

use super::write_gauge;
use crate::engine::idempotency::{IdempotencyLimits, IdempotencyUsage};

pub(super) fn append(body: &mut String, usage: IdempotencyUsage, limits: IdempotencyLimits) {
    for (name, used, limit) in [
        ("records", usage.global_records, limits.global_max_records),
        (
            "response_bytes",
            usage.global_response_bytes,
            limits.global_max_response_bytes,
        ),
    ] {
        writeln!(
            body,
            "ironcrew_idempotency_global_usage{{resource=\"{name}\"}} {used}"
        )
        .unwrap();
        writeln!(
            body,
            "ironcrew_idempotency_global_limit{{resource=\"{name}\"}} {limit}"
        )
        .unwrap();
    }
    write_gauge(
        body,
        "ironcrew_idempotency_global_in_flight",
        usage.global_in_flight,
    );
    for (name, used, limit) in [
        (
            "records",
            usage.max_principal_records,
            limits.principal_max_records,
        ),
        (
            "in_flight",
            usage.max_principal_in_flight,
            limits.principal_max_in_flight,
        ),
        (
            "response_bytes",
            usage.max_principal_response_bytes,
            limits.principal_max_response_bytes,
        ),
    ] {
        writeln!(
            body,
            "ironcrew_idempotency_max_principal_usage{{resource=\"{name}\"}} {used}"
        )
        .unwrap();
        writeln!(
            body,
            "ironcrew_idempotency_principal_limit{{resource=\"{name}\"}} {limit}"
        )
        .unwrap();
    }
    write_gauge(
        body,
        "ironcrew_idempotency_principals",
        usage.principal_count,
    );
    for (threshold, count) in [
        ("80", usage.principals_at_or_above_80_percent),
        ("90", usage.principals_at_or_above_90_percent),
        ("100", usage.principals_at_or_above_100_percent),
    ] {
        writeln!(
            body,
            "ironcrew_idempotency_saturated_principals{{threshold_percent=\"{threshold}\"}} {count}"
        )
        .unwrap();
    }
}
