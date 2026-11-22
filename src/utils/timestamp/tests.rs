//! Parity with the `chrono` 0.4.45 implementation these forms replaced.
//!
//! Every expected value below was recorded from `chrono` before the switch
//! (IC-057): `DateTime<Utc>::to_rfc3339`, `to_rfc3339_opts(Micros, true)`,
//! `DateTime::parse_from_rfc3339` followed by `to_rfc3339` in UTC or the
//! error's text, the lease-deadline expression and `NaiveDate` validation.

use std::time::Duration;

use super::*;

fn instant(seconds: i64, nanos: u32) -> Timestamp {
    Timestamp { seconds, nanos }
}

const SATURATED_RFC3339: &str = "+262142-12-31T23:59:59.999999999+00:00";

#[test]
fn written_forms_match_chrono() {
    let cases = [
        (
            (1_784_368_800, 0),
            "2026-07-18T10:00:00+00:00",
            "2026-07-18T10:00:00.000000Z",
        ),
        (
            (1_784_368_800, 120_000_000),
            "2026-07-18T10:00:00.120+00:00",
            "2026-07-18T10:00:00.120000Z",
        ),
        (
            (1_784_368_800, 123_000),
            "2026-07-18T10:00:00.000123+00:00",
            "2026-07-18T10:00:00.000123Z",
        ),
        (
            (1_784_368_800, 1),
            "2026-07-18T10:00:00.000000001+00:00",
            "2026-07-18T10:00:00.000000Z",
        ),
        (
            (1_784_368_800, 999_999_999),
            "2026-07-18T10:00:00.999999999+00:00",
            "2026-07-18T10:00:00.999999Z",
        ),
        (
            (0, 0),
            "1970-01-01T00:00:00+00:00",
            "1970-01-01T00:00:00.000000Z",
        ),
        (
            (-62_167_219_200, 0),
            "0000-01-01T00:00:00+00:00",
            "0000-01-01T00:00:00.000000Z",
        ),
        (
            (253_402_300_799, 0),
            "9999-12-31T23:59:59+00:00",
            "9999-12-31T23:59:59.000000Z",
        ),
        (
            (253_402_387_139, 999_999_999),
            "+10000-01-01T23:58:59.999999999+00:00",
            "+10000-01-01T23:58:59.999999Z",
        ),
        (
            (-62_167_305_540, 0),
            "-0001-12-31T00:01:00+00:00",
            "-0001-12-31T00:01:00.000000Z",
        ),
        (
            (-8_334_601_228_800, 0),
            "-262143-01-01T00:00:00+00:00",
            "-262143-01-01T00:00:00.000000Z",
        ),
        (
            (8_210_266_876_799, 999_999_999),
            SATURATED_RFC3339,
            "+262142-12-31T23:59:59.999999Z",
        ),
    ];
    for ((seconds, nanos), rfc3339, micros_z) in cases {
        let value = instant(seconds, nanos);
        assert_eq!(to_rfc3339(value), rfc3339);
        assert_eq!(to_rfc3339_micros_z(value), micros_z);
    }
}

#[test]
fn accepted_inputs_match_chrono() {
    let cases = [
        ("2026-07-18T10:00:00Z", "2026-07-18T10:00:00+00:00"),
        ("2026-07-18T10:00:00.1Z", "2026-07-18T10:00:00.100+00:00"),
        (
            "2026-07-18T10:00:00.123456789123+05:30",
            "2026-07-18T04:30:00.123456789+00:00",
        ),
        (
            "2026-07-18 10:00:00\u{2212}02:15",
            "2026-07-18T12:15:00+00:00",
        ),
        ("2026-07-18t10:00:00z", "2026-07-18T10:00:00+00:00"),
        ("0000-01-01T00:00:00+23:59", "-0001-12-31T00:01:00+00:00"),
        ("2024-02-29T00:00:00Z", "2024-02-29T00:00:00+00:00"),
        ("9999-12-31T23:59:59Z", "9999-12-31T23:59:59+00:00"),
        (
            "9999-12-31T23:59:59.999999999-23:59",
            "+10000-01-01T23:58:59.999999999+00:00",
        ),
        ("1970-01-01T00:00:00+00:00", "1970-01-01T00:00:00+00:00"),
        // Every form IronCrew writes reads back unchanged.
        (
            "2026-07-18T10:00:00.120000Z",
            "2026-07-18T10:00:00.120+00:00",
        ),
        (
            "2026-07-18T10:00:00.000000001+00:00",
            "2026-07-18T10:00:00.000000001+00:00",
        ),
    ];
    for (input, expected) in cases {
        assert_eq!(
            to_rfc3339(parse_rfc3339(input).unwrap()),
            expected,
            "{input:?}"
        );
    }
}

#[test]
fn refused_inputs_report_chrono_errors() {
    let cases = [
        ("2026-02-29T00:00:00Z", "input is out of range"),
        ("2026-07-18T10:00:00", "premature end of input"),
        (
            "2026-07-18T10:00:00+0530",
            "input contains invalid characters",
        ),
        ("2026-07-18T10:00:00+05", "premature end of input"),
        ("2026-07-18T10:00:00Z ", "trailing input"),
        (" 2026-07-18T10:00:00Z", "input contains invalid characters"),
        ("2026-07-18T24:00:00Z", "input is out of range"),
        ("2026-07-18T10:00:00+24:00", "input is out of range"),
        ("2026-07-18T10:00:00+05:60", "input is out of range"),
        ("2026-07-18T10:00:00.Z", "input contains invalid characters"),
        ("2026-7-18T10:00:00Z", "input contains invalid characters"),
        ("+2026-07-18T10:00:00Z", "input contains invalid characters"),
        ("", "premature end of input"),
        ("20260718T100000Z", "premature end of input"),
    ];
    for (input, expected) in cases {
        assert_eq!(
            parse_rfc3339(input).unwrap_err().to_string(),
            expected,
            "{input:?}"
        );
    }
}

#[test]
fn leap_seconds_are_the_one_difference_from_chrono() {
    // `chrono` kept a leap second; it now reads as the instant just before.
    assert_eq!(
        to_rfc3339(parse_rfc3339("2016-12-31T23:59:60Z").unwrap()),
        "2016-12-31T23:59:59.999999999+00:00"
    );
    assert_eq!(
        to_rfc3339(parse_rfc3339("2016-12-31T23:59:60.5+01:00").unwrap()),
        "2016-12-31T22:59:59.999999999+00:00"
    );
}

#[test]
fn lease_deadlines_match_chrono() {
    let at = |text| parse_rfc3339(text).unwrap();
    let cases = [
        (
            "2026-07-18T10:00:00Z",
            Duration::from_secs(60),
            "2026-07-18T10:01:00+00:00",
        ),
        (
            "2026-07-18T10:00:00.123456789Z",
            Duration::from_millis(1_500),
            "2026-07-18T10:00:01.623456789+00:00",
        ),
        (
            "2026-12-31T23:59:30.5+00:00",
            Duration::from_secs(45),
            "2027-01-01T00:00:15.500+00:00",
        ),
        // Longer than `chrono` could hold: counted as 60 seconds.
        (
            "2026-07-18T10:00:00Z",
            Duration::from_secs(u64::MAX),
            "2026-07-18T10:01:00+00:00",
        ),
        (
            "2026-07-18T10:00:00Z",
            Duration::new(9_223_372_036_854_775, 807_000_001),
            "2026-07-18T10:01:00+00:00",
        ),
        // The longest `chrono` duration overflows and saturates.
        (
            "2026-07-18T10:00:00Z",
            Duration::new(9_223_372_036_854_775, 807_000_000),
            SATURATED_RFC3339,
        ),
    ];
    for (now, ttl, expected) in cases {
        assert_eq!(lease_deadline(at(now), ttl), expected, "{now} + {ttl:?}");
    }
    assert_eq!(
        saturating_rfc3339(checked_add_seconds(at("2026-07-18T10:00:00Z"), 86_400 * 30)),
        "2026-08-17T10:00:00+00:00"
    );
    assert_eq!(
        saturating_rfc3339(checked_add_seconds(
            at("9999-12-31T23:59:59Z"),
            86_400 * 365
        )),
        "+10000-12-30T23:59:59+00:00"
    );
    assert_eq!(checked_add_seconds(Timestamp::MAX, 1), None);
    assert_eq!(checked_add(Timestamp::MAX, Duration::from_nanos(1)), None);
    assert_eq!(
        checked_add_seconds(instant(-8_334_601_228_800, 0), -1),
        None
    );
}

#[test]
fn durations_past_chrono_are_refused_with_its_error_text() {
    let longest = Duration::new(9_223_372_036_854_775, 807_000_000);
    assert!(signed_duration(longest).is_ok());
    assert_eq!(
        signed_duration(longest + Duration::from_nanos(1))
            .unwrap_err()
            .to_string(),
        "Source duration value is out of range for the target type"
    );
}

#[test]
fn elapsed_milliseconds_truncate_toward_zero_like_chrono() {
    let at = |text| parse_rfc3339(text).unwrap();
    let base = at("2026-07-18T10:00:00Z");
    let later = at("2026-07-18T10:00:00.0015Z");
    assert_eq!(millis_between(base, later), 1);
    assert_eq!(millis_between(later, base), -1);
    assert_eq!(elapsed_millis(base, later), 1);
    assert_eq!(elapsed_millis(later, base), 0);
    assert_eq!(
        millis_between(
            at("2026-07-18T09:59:59.999Z"),
            at("2026-07-18T12:00:00+02:00")
        ),
        1
    );
}

#[test]
fn calendar_dates_match_chrono() {
    let cases = [
        ((2026, 2, 29), false),
        ((2024, 2, 29), true),
        ((0, 2, 29), true),
        ((1900, 2, 29), false),
        ((2026, 4, 31), false),
        ((2026, 13, 1), false),
        ((2026, 0, 10), false),
        ((9999, 12, 31), true),
    ];
    for ((year, month, day), expected) in cases {
        assert_eq!(
            is_valid_date(year, month, day),
            expected,
            "{year}-{month}-{day}"
        );
    }
}

#[test]
fn the_current_instant_round_trips() {
    let text = now_rfc3339();
    assert!(text.ends_with("+00:00"), "{text}");
    assert_eq!(to_rfc3339(parse_rfc3339(&text).unwrap()), text);
}
