//! `chrono` 0.4.45's RFC 3339 reader, ported check for check (IC-057).

use std::fmt;

use super::{Timestamp, calendar, is_valid_date};

/// Why an instant could not be read. The text is `chrono`'s, unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParseError {
    OutOfRange,
    Invalid,
    TooShort,
    TooLong,
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::OutOfRange => "input is out of range",
            Self::Invalid => "input contains invalid characters",
            Self::TooShort => "premature end of input",
            Self::TooLong => "trailing input",
        })
    }
}

impl std::error::Error for ParseError {}

/// Read an RFC 3339 instant with the grammar, the order of checks and the
/// error text of `chrono`'s `DateTime::parse_from_rfc3339`.
pub fn parse_rfc3339(text: &str) -> Result<Timestamp, ParseError> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return Err(ParseError::TooShort);
    }
    let digit = |index: usize| {
        let byte = bytes[index];
        if byte.is_ascii_digit() {
            Ok(i32::from(byte - b'0'))
        } else {
            Err(ParseError::Invalid)
        }
    };
    let separator = |index: usize, allowed: &[u8]| {
        if allowed.contains(&bytes[index]) {
            Ok(())
        } else {
            Err(ParseError::Invalid)
        }
    };
    let year = digit(0)? * 1_000 + digit(1)? * 100 + digit(2)? * 10 + digit(3)?;
    separator(4, b"-")?;
    let month = digit(5)? * 10 + digit(6)?;
    separator(7, b"-")?;
    let day = digit(8)? * 10 + digit(9)?;
    if !is_valid_date(i64::from(year), month as u32, day as u32) {
        return Err(ParseError::OutOfRange);
    }
    separator(10, b"Tt ")?;
    let hour = digit(11)? * 10 + digit(12)?;
    separator(13, b":")?;
    let minute = digit(14)? * 10 + digit(15)?;
    separator(16, b":")?;
    let second = digit(17)? * 10 + digit(18)?;
    let mut cursor = Cursor(&bytes[19..]);
    let fraction = if cursor.0.first() == Some(&b'.') {
        cursor.0 = &cursor.0[1..];
        cursor.fraction()?
    } else {
        0
    };
    // `chrono` read a leap second as a nanosecond count past one second;
    // `Timestamp` has none, so it becomes the last nanosecond before it.
    let (second, nanos) = if second == 60 {
        (59, 999_999_999)
    } else {
        (second, fraction)
    };
    if hour > 23 || minute > 59 || second > 59 {
        return Err(ParseError::OutOfRange);
    }
    let offset = cursor.offset()?;
    if !cursor.0.is_empty() {
        return Err(ParseError::TooLong);
    }
    if offset.abs() >= 86_400 {
        return Err(ParseError::OutOfRange);
    }
    let days = calendar::days_from_civil(i64::from(year), month as u32, day as u32);
    let seconds = days * 86_400 + i64::from(hour * 3_600 + minute * 60 + second - offset);
    // Years 0000 to 9999 with a 23:59 offset lie well inside the range.
    Ok(Timestamp {
        seconds,
        nanos: nanos as u32,
    })
}

struct Cursor<'a>(&'a [u8]);

impl Cursor<'_> {
    /// `chrono`'s `scan::number`: `min` to `max` ASCII digits.
    fn number(&mut self, min: usize, max: usize) -> Result<i64, ParseError> {
        if self.0.len() < min {
            return Err(ParseError::TooShort);
        }
        let mut value = 0i64;
        for (index, &byte) in self.0.iter().take(max).enumerate() {
            if !byte.is_ascii_digit() {
                if index < min {
                    return Err(ParseError::Invalid);
                }
                self.0 = &self.0[index..];
                return Ok(value);
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        self.0 = &self.0[max.min(self.0.len())..];
        Ok(value)
    }

    fn expect(&mut self, expected: u8) -> Result<(), ParseError> {
        match self.0.first() {
            Some(&byte) if byte == expected => {
                self.0 = &self.0[1..];
                Ok(())
            }
            Some(_) => Err(ParseError::Invalid),
            None => Err(ParseError::TooShort),
        }
    }

    /// `chrono`'s `scan::nanosecond`: one or more digits, the first nine kept.
    fn fraction(&mut self) -> Result<i32, ParseError> {
        let before = self.0.len();
        let value = self.number(1, 9)?;
        let digits = before - self.0.len();
        let scaled = value * 10i64.pow(9 - digits as u32);
        let skipped = self
            .0
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        self.0 = &self.0[skipped..];
        Ok(scaled as i32)
    }

    /// `chrono`'s RFC 3339 offset: `Z`, `z`, or a sign, two hour digits, a
    /// colon and two minute digits.
    fn offset(&mut self) -> Result<i32, ParseError> {
        const UNICODE_MINUS: &[u8] = "\u{2212}".as_bytes();
        let negative = match self.0 {
            [b'Z' | b'z', rest @ ..] => {
                self.0 = rest;
                return Ok(0);
            }
            [b'+', rest @ ..] => {
                self.0 = rest;
                false
            }
            [b'-', rest @ ..] => {
                self.0 = rest;
                true
            }
            bytes if bytes.starts_with(UNICODE_MINUS) => {
                self.0 = &bytes[UNICODE_MINUS.len()..];
                true
            }
            [] => return Err(ParseError::TooShort),
            _ => return Err(ParseError::Invalid),
        };
        let hours = match self.0 {
            [high @ b'0'..=b'9', low @ b'0'..=b'9', ..] => {
                i32::from((high - b'0') * 10 + (low - b'0'))
            }
            [_, _, ..] => return Err(ParseError::Invalid),
            _ => return Err(ParseError::TooShort),
        };
        self.0 = &self.0[2..];
        self.expect(b':')?;
        let minutes = match self.0 {
            [high @ b'0'..=b'5', low @ b'0'..=b'9', ..] => {
                i32::from((high - b'0') * 10 + (low - b'0'))
            }
            [b'6'..=b'9', b'0'..=b'9', ..] => return Err(ParseError::OutOfRange),
            [_, _, ..] => return Err(ParseError::Invalid),
            _ => return Err(ParseError::TooShort),
        };
        self.0 = &self.0[2..];
        let seconds = hours * 3_600 + minutes * 60;
        Ok(if negative { -seconds } else { seconds })
    }
}
