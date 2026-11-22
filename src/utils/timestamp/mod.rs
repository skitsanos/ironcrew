//! The instants IronCrew stores, returns and logs, and their text (IC-057).
//!
//! IronCrew first wrote its timestamps with `chrono`. [`Timestamp`] replaces
//! `chrono`'s `DateTime<Utc>` with the same range (the years -262143 to 262142),
//! and every form stays byte for byte what it was:
//!
//! | form | example | used by |
//! | --- | --- | --- |
//! | [`to_rfc3339`] | `2026-01-02T03:04:05.120+00:00` | run, conversation, idempotency, HITL and audit records; API bodies; Lua `now_rfc3339()` |
//! | [`to_rfc3339_micros_z`] | `2026-01-02T03:04:05.120000Z` | PostgreSQL canonical timestamps |
//!
//! [`to_rfc3339`] writes no fraction for a whole second and otherwise three,
//! six or nine digits, whichever is the shortest that loses nothing.
//! [`parse_rfc3339`] reads exactly what `chrono`'s `parse_from_rfc3339` read,
//! with the same error text. The one difference: a leap second (`:60`), which
//! IronCrew never writes, reads as the last nanosecond of the preceding
//! second instead of a separate instant.

use std::fmt::Write as _;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod calendar;
pub mod parse;

pub use parse::parse_rfc3339;

/// An instant in UTC with nanosecond precision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp {
    seconds: i64,
    nanos: u32,
}

/// `chrono`'s `DateTime::<Utc>::MIN_UTC`: -262143-01-01T00:00:00Z.
const MIN_SECONDS: i64 = -8_334_601_228_800;
/// `chrono`'s `DateTime::<Utc>::MAX_UTC`, to the second.
const MAX_SECONDS: i64 = 8_210_266_876_799;
const NANOS_PER_SECOND: i128 = 1_000_000_000;

impl Timestamp {
    /// The latest instant, `+262142-12-31T23:59:59.999999999Z`, as in `chrono`.
    pub const MAX: Self = Self {
        seconds: MAX_SECONDS,
        nanos: 999_999_999,
    };

    fn from_nanos(total: i128) -> Option<Self> {
        let seconds = i64::try_from(total.div_euclid(NANOS_PER_SECOND)).ok()?;
        if !(MIN_SECONDS..=MAX_SECONDS).contains(&seconds) {
            return None;
        }
        Some(Self {
            seconds,
            nanos: total.rem_euclid(NANOS_PER_SECOND) as u32,
        })
    }

    fn as_nanos(self) -> i128 {
        i128::from(self.seconds) * NANOS_PER_SECOND + i128::from(self.nanos)
    }
}

impl std::str::FromStr for Timestamp {
    type Err = parse::ParseError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        parse_rfc3339(text)
    }
}

/// The current instant from the system clock.
pub fn now() -> Timestamp {
    let total = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(after) => after.as_nanos() as i128,
        Err(before) => -(before.duration().as_nanos() as i128),
    };
    Timestamp::from_nanos(total).unwrap_or(Timestamp::MAX)
}

/// The current instant in the [`to_rfc3339`] form.
pub fn now_rfc3339() -> String {
    to_rfc3339(now())
}

/// The form `chrono`'s `DateTime<Utc>::to_rfc3339` wrote: shortest exact
/// fraction and `+00:00`.
pub fn to_rfc3339(instant: Timestamp) -> String {
    let mut text = write_seconds(instant);
    let nanos = instant.nanos;
    // Writing to a `String` cannot fail.
    let _ = if nanos == 0 {
        Ok(())
    } else if nanos.is_multiple_of(1_000_000) {
        write!(text, ".{:03}", nanos / 1_000_000)
    } else if nanos.is_multiple_of(1_000) {
        write!(text, ".{:06}", nanos / 1_000)
    } else {
        write!(text, ".{nanos:09}")
    };
    text.push_str("+00:00");
    text
}

/// The form `chrono`'s `to_rfc3339_opts(SecondsFormat::Micros, true)` wrote:
/// six fraction digits, truncated, and `Z`.
pub fn to_rfc3339_micros_z(instant: Timestamp) -> String {
    let mut text = write_seconds(instant);
    let _ = write!(text, ".{:06}Z", instant.nanos / 1_000);
    text
}

fn write_seconds(instant: Timestamp) -> String {
    let (days, second_of_day) = (
        instant.seconds.div_euclid(86_400),
        instant.seconds.rem_euclid(86_400),
    );
    let (year, month, day) = calendar::civil_from_days(days);
    let mut text = String::with_capacity(40);
    let _ = if (0..=9_999).contains(&year) {
        write!(text, "{year:04}")
    } else {
        write!(text, "{year:+05}")
    };
    let _ = write!(
        text,
        "-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        second_of_day / 3_600,
        second_of_day / 60 % 60,
        second_of_day % 60
    );
    text
}

/// `instant + seconds`, or `None` past the supported range.
pub fn checked_add_seconds(instant: Timestamp, seconds: i64) -> Option<Timestamp> {
    Timestamp::from_nanos(instant.as_nanos() + i128::from(seconds) * NANOS_PER_SECOND)
}

/// `instant + duration`, or `None` past the supported range.
pub fn checked_add(instant: Timestamp, duration: Duration) -> Option<Timestamp> {
    Timestamp::from_nanos(instant.as_nanos() + duration.as_nanos() as i128)
}

/// The longest duration `chrono` held (`TimeDelta::MAX`): `i64::MAX` milliseconds.
const CHRONO_MAX_DURATION: Duration = Duration::new(i64::MAX as u64 / 1_000, 807_000_000);

/// `duration`, refused with `chrono`'s error text when it is longer than the
/// longest duration `chrono` held.
pub fn signed_duration(duration: Duration) -> Result<Duration, DurationOutOfRange> {
    if duration > CHRONO_MAX_DURATION {
        Err(DurationOutOfRange)
    } else {
        Ok(duration)
    }
}

/// A duration too long to add to an instant.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurationOutOfRange;

impl std::fmt::Display for DurationOutOfRange {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Source duration value is out of range for the target type")
    }
}

impl std::error::Error for DurationOutOfRange {}

/// A lease deadline `ttl` after `now` in the [`to_rfc3339`] form.
///
/// Kept exactly as written with `chrono`: a TTL longer than `chrono` could
/// hold counts as 60 seconds, and a deadline past the range saturates to
/// [`Timestamp::MAX`]. Lease TTLs are validated to 24 hours, so neither
/// branch is reachable from configuration.
pub fn lease_deadline(now: Timestamp, ttl: Duration) -> String {
    let ttl = signed_duration(ttl).unwrap_or(Duration::from_secs(60));
    saturating_rfc3339(checked_add(now, ttl))
}

/// [`to_rfc3339`] of `instant`, or of [`Timestamp::MAX`] when it is `None`.
pub fn saturating_rfc3339(instant: Option<Timestamp>) -> String {
    to_rfc3339(instant.unwrap_or(Timestamp::MAX))
}

/// Whole milliseconds from `start` to `end`, truncated toward zero; negative
/// when `end` is earlier.
pub fn millis_between(start: Timestamp, end: Timestamp) -> i64 {
    // The whole range spans less than 2^59 milliseconds.
    ((end.as_nanos() - start.as_nanos()) / 1_000_000) as i64
}

/// Whole milliseconds from `start` to `end`, or zero when `end` is earlier.
pub fn elapsed_millis(start: Timestamp, end: Timestamp) -> u64 {
    u64::try_from(millis_between(start, end)).unwrap_or(0)
}

/// Whether `year`-`month`-`day` names a day of the proleptic Gregorian
/// calendar.
pub fn is_valid_date(year: i64, month: u32, day: u32) -> bool {
    (1..=12).contains(&month) && (1..=calendar::days_in_month(year, month)).contains(&day)
}

#[cfg(test)]
mod tests;
