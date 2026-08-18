//! 5-field cron expression parser with next-fire computation.
//!
//! Supports the standard Unix cron syntax for the five fields:
//! `minute hour day-of-month month day-of-week`.
//!
//! Each field accepts: `*` (all values), a single value (`5`), a range
//! (`1-5`), a list (`1,3,5`), and a step (`*/2`, `1-10/2`). Lists may mix
//! ranges and single values (`1-3,7,10-12`).
//!
//! Day-of-week uses 0=Sunday … 6=Saturday (standard cron convention).
//!
//! The parser is pure (no I/O, no async); the scheduling engine calls
//! [`CronExpr::next_after`] to compute the next fire time from a cron
//! expression relative to a reference timestamp.

use std::fmt;

use anyhow::{Result, bail};
use time::OffsetDateTime;

/// A parsed cron expression with five bit-set fields.
///
/// Each field is a `u64` bitmask over the valid range for that field.
/// A set bit means "this value matches."
#[derive(Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: u64,
    hour: u64,
    dom: u64,
    month: u64,
    dow: u64,
    source: String,
}

impl CronExpr {
    /// Parse a 5-field cron expression.
    pub fn parse(expr: &str) -> Result<Self> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            bail!(
                "cron expression must have exactly 5 fields, got {}: '{expr}'",
                parts.len()
            );
        }
        let minute = parse_field(parts[0], 0, 59, "minute")?;
        let hour = parse_field(parts[1], 0, 23, "hour")?;
        let dom = parse_field(parts[2], 1, 31, "day-of-month")?;
        let month = parse_field(parts[3], 1, 12, "month")?;
        let dow = parse_field(parts[4], 0, 6, "day-of-week")?;
        Ok(Self {
            minute,
            hour,
            dom,
            month,
            dow,
            source: expr.to_string(),
        })
    }

    /// Compute the next fire time strictly after `after`.
    ///
    /// Uses field-level skipping: when the hour doesn't match, jumps to the
    /// next matching hour; when the day doesn't match, jumps to midnight of
    /// the next day. Only the minute field is scanned linearly (at most 60
    /// iterations per matching hour). This keeps even pathological expressions
    /// (e.g. Feb 29) fast.
    pub fn next_after(&self, after: OffsetDateTime) -> Result<OffsetDateTime> {
        let mut candidate = round_up_to_next_minute(after);
        let limit = after + time::Duration::days(366 * 5);
        loop {
            if candidate > limit {
                bail!(
                    "no fire time for cron '{}' within 5 years of {}",
                    self.source,
                    after
                );
            }
            // Check month first — if wrong, jump to the first of the next month.
            if !bit(self.month, candidate.month() as u8 as u64) {
                candidate = first_of_next_month(candidate)?;
                continue;
            }
            // Check day (dom/dow OR semantics).
            if !self.day_matches(candidate) {
                candidate = midnight_of_next_day(candidate);
                continue;
            }
            // Check hour — if wrong, advance one hour (resetting minute/second)
            if !bit(self.hour, candidate.hour() as u64) {
                candidate = candidate.replace_minute(0).unwrap_or(candidate);
                candidate = candidate + time::Duration::hours(1);
                continue;
            }
            // Minute: scan linearly within this hour (at most 60 steps).
            if bit(self.minute, candidate.minute() as u64) {
                return Ok(candidate);
            }
            candidate = candidate + time::Duration::minutes(1);
        }
    }

    /// Day-of-month and day-of-week match with cron OR semantics.
    fn day_matches(&self, dt: OffsetDateTime) -> bool {
        let dom_match = bit(self.dom, dt.day() as u64);
        let dow_match = bit(self.dow, weekday_to_cron_dow(dt.weekday()));
        let dom_restricted = self.dom != dom_full_set();
        let dow_restricted = self.dow != dow_full_set();
        if dom_restricted && dow_restricted {
            dom_match || dow_match
        } else {
            dom_match && dow_match
        }
    }
}

impl fmt::Display for CronExpr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.source)
    }
}

impl fmt::Debug for CronExpr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.debug_struct("CronExpr")
            .field("source", &self.source)
            .field("minute", &format_bits(self.minute, 0, 59))
            .field("hour", &format_bits(self.hour, 0, 23))
            .field("dom", &format_bits(self.dom, 1, 31))
            .field("month", &format_bits(self.month, 1, 12))
            .field("dow", &format_bits(self.dow, 0, 6))
            .finish()
    }
}

fn format_bits(mask: u64, lo: u8, hi: u8) -> Vec<u8> {
    (lo..=hi).filter(|&v| bit(mask, v as u64)).collect()
}

// --- bitmask helpers ---

#[inline]
fn bit(mask: u64, val: u64) -> bool {
    mask & (1 << val) != 0
}

/// The full set for day-of-month (bits 1..31 set).
fn dom_full_set() -> u64 {
    ((1u64 << 32) - 1) & !1
}

/// The full set for day-of-week (bits 0..6 set).
fn dow_full_set() -> u64 {
    0b1111111
}

// --- field parsing ---

fn parse_field(field: &str, lo: u8, hi: u8, name: &str) -> Result<u64> {
    let mut mask = 0u64;
    for part in field.split(',') {
        mask |= parse_part(part, lo, hi, name)?;
    }
    if mask == 0 {
        bail!("cron field '{name}' ('{field}') produced no matching values");
    }
    Ok(mask)
}

fn parse_part(part: &str, lo: u8, hi: u8, name: &str) -> Result<u64> {
    let (range_str, step) = match part.find('/') {
        Some(pos) => {
            let step: u8 = part[pos + 1..]
                .parse()
                .map_err(|_| anyhow::anyhow!("invalid step in cron {name} field: '{part}'"))?;
            if step == 0 {
                bail!("cron {name} field step must be > 0: '{part}'");
            }
            (&part[..pos], Some(step))
        }
        None => (part, None),
    };
    let (start, end) = if range_str == "*" {
        (lo, hi)
    } else if let Some(pos) = range_str.find('-') {
        let s = parse_value(&range_str[..pos], lo, hi, name)?;
        let e = parse_value(&range_str[pos + 1..], lo, hi, name)?;
        if s > e {
            bail!("cron {name} field range start > end: '{range_str}'");
        }
        (s, e)
    } else {
        let v = parse_value(range_str, lo, hi, name)?;
        let end = if step.is_some() { hi } else { v };
        (v, end)
    };
    let step = step.unwrap_or(1);
    let mut mask = 0u64;
    let mut cur = start;
    while cur <= end {
        mask |= 1 << cur;
        cur = match cur.checked_add(step) {
            Some(v) => v,
            None => break,
        };
    }
    if mask == 0 {
        bail!("cron {name} field component '{part}' produced no values");
    }
    Ok(mask)
}

fn parse_value(s: &str, lo: u8, hi: u8, name: &str) -> Result<u8> {
    let v: u8 = s
        .parse()
        .map_err(|_| anyhow::anyhow!("invalid value '{s}' in cron {name} field"))?;
    if v < lo || v > hi {
        bail!("cron {name} field value {v} out of range [{lo}, {hi}]");
    }
    Ok(v)
}

// --- time helpers ---

fn weekday_to_cron_dow(wd: time::Weekday) -> u64 {
    match wd {
        time::Weekday::Sunday => 0,
        time::Weekday::Monday => 1,
        time::Weekday::Tuesday => 2,
        time::Weekday::Wednesday => 3,
        time::Weekday::Thursday => 4,
        time::Weekday::Friday => 5,
        time::Weekday::Saturday => 6,
    }
}

fn round_up_to_next_minute(dt: OffsetDateTime) -> OffsetDateTime {
    let truncated = dt
        .replace_second(0)
        .unwrap_or(dt)
        .replace_millisecond(0)
        .unwrap_or(dt);
    truncated + time::Duration::minutes(1)
}

/// Jump to 00:00 of the first day of the next month.
///
/// Note: v1 is UTC-only; `assume_utc()` discards the input offset.
fn first_of_next_month(dt: OffsetDateTime) -> Result<OffsetDateTime> {
    let (year, month) = if dt.month() as u8 == 12 {
        (dt.year() + 1, time::Month::January)
    } else {
        (dt.year(), time::Month::try_from(dt.month() as u8 + 1)?)
    };
    let date = time::Date::from_calendar_date(year, month, 1)?;
    Ok(date.with_time(time::Time::MIDNIGHT).assume_utc())
}

/// Jump to 00:00 of the next day.
///
/// Note: v1 is UTC-only; `assume_utc()` discards the input offset.
fn midnight_of_next_day(dt: OffsetDateTime) -> OffsetDateTime {
    (dt.date() + time::Duration::DAY)
        .with_time(time::Time::MIDNIGHT)
        .assume_utc()
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn parse_basic_fields() {
        let expr = CronExpr::parse("0 9 * * 1-5").unwrap();
        assert_eq!(expr.source, "0 9 * * 1-5");
    }

    #[test]
    fn parse_rejects_wrong_field_count() {
        assert!(CronExpr::parse("0 9 * *").is_err());
        assert!(CronExpr::parse("0 9 * * 1 2").is_err());
    }

    #[test]
    fn parse_rejects_out_of_range() {
        assert!(CronExpr::parse("60 9 * * *").is_err());
        assert!(CronExpr::parse("0 24 * * *").is_err());
        assert!(CronExpr::parse("0 9 32 * *").is_err());
        assert!(CronExpr::parse("0 9 * 13 *").is_err());
        assert!(CronExpr::parse("0 9 * * 7").is_err());
    }

    #[test]
    fn parse_rejects_reverse_range() {
        assert!(CronExpr::parse("0 9 * * 5-1").is_err());
    }

    #[test]
    fn parse_rejects_zero_step() {
        assert!(CronExpr::parse("*/0 * * * *").is_err());
    }

    #[test]
    fn step_every_2_hours() {
        let expr = CronExpr::parse("0 */2 * * *").unwrap();
        let after = datetime!(2024-01-15 01:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 02:00 UTC)
        );
    }

    #[test]
    fn list_field() {
        let expr = CronExpr::parse("0 1,3,5 * * *").unwrap();
        let after = datetime!(2024-01-15 00:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 01:00 UTC)
        );
        let after = datetime!(2024-01-15 01:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 03:00 UTC)
        );
    }

    #[test]
    fn range_with_step() {
        let expr = CronExpr::parse("0 9-17/2 * * 1-5").unwrap();
        let after = datetime!(2024-01-15 08:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 09:00 UTC)
        );
    }

    #[test]
    fn next_after_weekday_expression() {
        // "0 9 * * 1-5" = 09:00 Monday-Friday; Friday 17:00 -> Monday 09:00
        let expr = CronExpr::parse("0 9 * * 1-5").unwrap();
        let friday_evening = datetime!(2024-01-12 17:00 UTC);
        assert_eq!(
            expr.next_after(friday_evening).unwrap(),
            datetime!(2024-01-15 09:00 UTC)
        );
    }

    #[test]
    fn next_after_every_minute() {
        let expr = CronExpr::parse("* * * * *").unwrap();
        let after = datetime!(2024-01-15 12:30:45 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 12:31 UTC)
        );
    }

    #[test]
    fn next_after_specific_minute() {
        let expr = CronExpr::parse("30 * * * *").unwrap();
        let after = datetime!(2024-01-15 12:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 12:30 UTC)
        );
    }

    #[test]
    fn next_after_advances_to_next_day() {
        let expr = CronExpr::parse("0 9 * * *").unwrap();
        let after = datetime!(2024-01-15 09:30 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-16 09:00 UTC)
        );
    }

    #[test]
    fn next_after_specific_month() {
        let expr = CronExpr::parse("0 0 1 6 *").unwrap();
        let after = datetime!(2024-01-15 12:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-06-01 00:00 UTC)
        );
    }

    #[test]
    fn dom_dow_or_semantics() {
        // "0 0 15 * 1" = midnight on the 15th OR every Monday.
        let expr = CronExpr::parse("0 0 15 * 1").unwrap();
        let after = datetime!(2024-01-14 00:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 00:00 UTC)
        );
        // Jan 15 is Monday. Next fire: Jan 22 (Monday), since dom=15 doesn't match.
        let next = expr.next_after(after).unwrap();
        assert_eq!(
            expr.next_after(next).unwrap(),
            datetime!(2024-01-22 00:00 UTC)
        );
    }

    #[test]
    fn impossible_expression_errors() {
        let expr = CronExpr::parse("0 0 31 2 *").unwrap();
        let after = datetime!(2024-01-01 00:00 UTC);
        assert!(expr.next_after(after).is_err());
    }

    #[test]
    fn february_29_leap_year() {
        let expr = CronExpr::parse("0 0 29 2 *").unwrap();
        let after = datetime!(2024-01-01 00:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-02-29 00:00 UTC)
        );
        // From Mar 2024, next is 2028 (2025/2026/2027 are not leap years).
        let after = datetime!(2024-03-01 00:00 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2028-02-29 00:00 UTC)
        );
    }

    #[test]
    fn second_precision_ignored() {
        let expr = CronExpr::parse("* * * * *").unwrap();
        let after = datetime!(2024-01-15 09:00:30 UTC);
        assert_eq!(
            expr.next_after(after).unwrap(),
            datetime!(2024-01-15 09:01 UTC)
        );
    }

    #[test]
    fn display_shows_source() {
        let expr = CronExpr::parse("0 9 * * 1-5").unwrap();
        assert_eq!(expr.to_string(), "0 9 * * 1-5");
    }
}
