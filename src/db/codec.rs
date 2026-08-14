//! Safe decoding for the `sqlx::Any` driver.
//!
//! Two `Any` decode paths are known to be wrong for this schema. Reading a
//! SQLite `INTEGER` through the typed scalar path narrows it to 32 bits, which
//! silently corrupts Telegram identifiers and Unix-millisecond timestamps. And a
//! genuine SQL `NULL` refuses to decode into `Option<String>` while also not
//! reporting itself through `ValueRef::is_null`.
//!
//! Every accessor here therefore expects the column to have been selected as
//! `CAST(... AS TEXT)` and parses the text itself. Errors name the column index
//! and the expected shape only: no stored value ever reaches the message.

use anyhow::{Result, bail};
use sqlx::{Row, TypeInfo, ValueRef, any::AnyRow};

/// Read a `CAST(... AS TEXT)` column that the schema declares `NOT NULL`.
pub fn text_at(row: &AnyRow, index: usize) -> Result<String> {
    match nullable_text_at(row, index)? {
        Some(value) => Ok(value),
        None => bail!("recap persistence: column {index} was NULL but is declared NOT NULL"),
    }
}

/// Read a `CAST(... AS TEXT)` column that may legitimately be SQL `NULL`.
///
/// Nullness is settled on the raw value's type before decoding, because the
/// `Any` driver neither reports it through `is_null` nor accepts it into an
/// `Option<String>`.
pub fn nullable_text_at(row: &AnyRow, index: usize) -> Result<Option<String>> {
    let raw = match row.try_get_raw(index) {
        Ok(raw) => raw,
        Err(_) => bail!("recap persistence: column {index} is missing from the result"),
    };
    if raw.is_null() || raw.type_info().name().eq_ignore_ascii_case("NULL") {
        return Ok(None);
    }
    match row.try_get::<String, _>(index) {
        Ok(value) => Ok(Some(value)),
        Err(_) => bail!("recap persistence: column {index} did not decode as text"),
    }
}

/// Parse the text rendering of a 64-bit integer column.
///
/// Split out from [`i64_at`] so both engines' renderings can be exercised
/// without a live server.
fn parse_i64(index: usize, raw: &str) -> Result<i64> {
    match raw.trim().parse::<i64>() {
        Ok(value) => Ok(value),
        Err(_) => bail!("recap persistence: column {index} is not a 64-bit integer"),
    }
}

/// Parse the text rendering of a boolean column.
///
/// PostgreSQL renders a `BOOLEAN` as `true`/`false` and abbreviates it to
/// `t`/`f` in some contexts; SQLite stores the same column as `1`/`0`. Both
/// spellings are accepted, and nothing else is.
fn parse_bool(index: usize, raw: &str) -> Result<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "t" | "true" => Ok(true),
        "0" | "f" | "false" => Ok(false),
        _ => bail!("recap persistence: column {index} is not a boolean"),
    }
}

/// Read a 64-bit integer column selected as text.
pub fn i64_at(row: &AnyRow, index: usize) -> Result<i64> {
    parse_i64(index, &text_at(row, index)?)
}

/// Read a boolean column selected as text.
pub fn bool_at(row: &AnyRow, index: usize) -> Result<bool> {
    parse_bool(index, &text_at(row, index)?)
}

/// A fresh repository-generated identifier.
pub fn new_identifier() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// The current instant in Unix milliseconds, as every parity table stores it.
pub fn now_unix_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::{parse_bool, parse_i64};

    #[test]
    fn both_engines_boolean_renderings_are_accepted() {
        for raw in ["true", "TRUE", "True", "t", "T", "1", " true ", "\t1\n"] {
            assert!(
                parse_bool(4, raw).unwrap_or_else(|error| panic!("{raw:?}: {error}")),
                "{raw:?} must read as true"
            );
        }
        for raw in ["false", "FALSE", "False", "f", "F", "0", " false ", "\t0\n"] {
            assert!(
                !parse_bool(4, raw).unwrap_or_else(|error| panic!("{raw:?}: {error}")),
                "{raw:?} must read as false"
            );
        }
    }

    /// A value no fixed message could contain by coincidence.
    const SENTINEL: &str = "sentinel-stored-value-must-not-leak";

    #[test]
    fn a_non_boolean_rendering_is_rejected_without_quoting_it() {
        const EXPECTED: &str = "recap persistence: column 4 is not a boolean";

        for raw in ["", "yes", "no", "2", "-1", "null", "TRUEISH", SENTINEL] {
            let error = parse_bool(4, raw)
                .expect_err("only the two engines' spellings are accepted")
                .to_string();
            // A message identical for every input cannot be quoting any of them.
            assert_eq!(error, EXPECTED, "the message must not vary with the input");
        }
        assert!(
            !parse_bool(4, SENTINEL)
                .expect_err("rejected")
                .to_string()
                .contains("sentinel"),
            "the stored value must never reach the message"
        );
    }

    #[test]
    fn signed_64_bit_extremes_survive_the_text_path() {
        for value in [
            i64::MIN,
            i64::MIN + 1,
            i64::from(i32::MIN) - 1,
            -1_001_234_567_890,
            -1,
            0,
            1,
            1_700_000_000_000,
            i64::from(i32::MAX) + 1,
            i64::MAX - 1,
            i64::MAX,
        ] {
            assert_eq!(
                parse_i64(2, &value.to_string()).unwrap_or_else(|error| panic!("{value}: {error}")),
                value
            );
        }
        // PostgreSQL pads nothing, but a cast can leave surrounding whitespace.
        assert_eq!(parse_i64(2, "  -42\n").expect("trimmed"), -42);
    }

    #[test]
    fn a_malformed_integer_is_rejected_without_quoting_the_value() {
        const EXPECTED: &str = "recap persistence: column 2 is not a 64-bit integer";

        for raw in [
            "",
            "abc",
            "1.5",
            "0x10",
            // One past each 64-bit bound.
            "9223372036854775808",
            "-9223372036854775809",
            "1 2",
            SENTINEL,
        ] {
            let error = parse_i64(2, raw)
                .expect_err("only a 64-bit integer is accepted")
                .to_string();
            assert_eq!(error, EXPECTED, "the message must not vary with the input");
        }
        assert!(
            !parse_i64(2, SENTINEL)
                .expect_err("rejected")
                .to_string()
                .contains("sentinel"),
            "the stored value must never reach the message"
        );
    }
}
