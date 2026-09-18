//! Time and count rendering shared by the CLI status views.
//!
//! The rule these helpers encode: arithmetic belongs to the system, not the
//! reader. A bare epoch or a 10-digit token count forces every reader to
//! convert in their head (and timezone-less vendor stamps have already caused
//! a nine-hour misread); these render finished, timezone-anchored strings.

use time::format_description::FormatItem;
use time::macros::format_description;
use time::{OffsetDateTime, UtcOffset};

/// RFC 3339 UTC, second precision, `Z` suffix — the same shape tracing emits
/// (`2026-08-23T19:05:07Z`), so status timestamps and log lines sort and read
/// identically.
const UTC_SECONDS: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");

/// Day precision, for the absolute suffix on old relative stamps.
const UTC_DAY: &[FormatItem<'static>] = format_description!("[year]-[month]-[day]");

/// Current wall clock as epoch seconds (0 on a pre-epoch clock); shared by
/// the CLI so every view anchors to the same clock source.
pub fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn to_utc(epoch_secs: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(epoch_secs)
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
        .to_offset(UtcOffset::UTC)
}

/// `2026-08-23T19:05:07Z` for an epoch-seconds stamp.
pub fn format_utc(epoch_secs: u64) -> String {
    to_utc(epoch_secs as i64)
        .format(&UTC_SECONDS)
        .unwrap_or_else(|_| "invalid".into())
}

/// Day-only UTC date (`2026-08-23`), used to anchor relative stamps past a day.
pub fn format_utc_day(epoch_secs: u64) -> String {
    to_utc(epoch_secs as i64)
        .format(&UTC_DAY)
        .unwrap_or_else(|_| "invalid".into())
}

/// Person-readable distance between `now_secs` and `epoch_secs`:
/// `just now` under a minute, `8m ago` / `in 41m` within a day, `2h ago`
/// within hours, and past a day the absolute date rides along
/// (`3d ago (2026-08-20)`) so the string cannot mislead once it leaves the
/// screen.
pub fn format_relative(now_secs: u64, epoch_secs: u64) -> String {
    let diff = now_secs.abs_diff(epoch_secs);
    let body = if diff < 60 {
        "just now".to_string()
    } else if diff < 60 * 60 {
        format!("{}m", diff / 60)
    } else if diff < 24 * 60 * 60 {
        format!("{}h", diff / (60 * 60))
    } else {
        format!(
            "{}d ({})",
            diff / (24 * 60 * 60),
            format_utc_day(epoch_secs)
        )
    };
    if diff < 60 {
        return body;
    }
    if epoch_secs <= now_secs {
        format!("{body} ago")
    } else {
        format!("in {body}")
    }
}
/// Parses a `YYYY-MM-DD` UTC date into epoch seconds at that day's
/// 00:00:00Z (journalctl's `--until 2026-09-15` reads the same way:
/// the named day's start). The inverse of [`format_utc_day`].
pub fn parse_utc_day(raw: &str) -> Result<u64, time::error::Parse> {
    let date = time::Date::parse(raw.trim(), &UTC_DAY)?;
    Ok(date
        .with_time(time::Time::MIDNIGHT)
        .assume_utc()
        .unix_timestamp() as u64)
}

/// Parses an RFC 3339 UTC stamp (`2026-08-23T19:05:07Z`) back into
/// epoch seconds; the inverse of [`format_utc`], for when a rendered
/// column has to be compared against a window again.
pub fn parse_utc(raw: &str) -> Result<u64, time::error::Parse> {
    // The format carries a literal `Z` (not an offset component), so the
    // offset-less PrimitiveDateTime is the right parse target; assume_utc
    // is exactly what that literal means.
    let t = time::PrimitiveDateTime::parse(raw.trim(), &UTC_SECONDS)?;
    Ok(t.assume_utc().unix_timestamp() as u64)
}

/// Compact magnitude for counts a human scans, not computes: `1.54G` for
/// 1542320459, `12.3M`, `845K`; below a thousand the raw number is already
/// the shortest honest form.
pub fn humanize_count(n: u64) -> String {
    let (scaled, unit) = if n >= 1_000_000_000 {
        (n as f64 / 1e9, 'G')
    } else if n >= 1_000_000 {
        (n as f64 / 1e6, 'M')
    } else if n >= 1_000 {
        (n as f64 / 1e3, 'K')
    } else {
        return n.to_string();
    };
    let mut text = format!("{scaled:.2}");
    if text.contains('.') {
        while text.ends_with('0') {
            text.pop();
        }
        if text.ends_with('.') {
            text.pop();
        }
    }
    format!("{text}{unit}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_shape_matches_tracing() {
        assert_eq!(format_utc(1_755_951_907), "2025-08-23T12:25:07Z");
        assert_eq!(parse_utc("2025-08-23T12:25:07Z").unwrap(), 1_755_951_907);
    }

    #[test]
    fn utc_day_shape() {
        assert_eq!(format_utc_day(1_755_951_907), "2025-08-23");
    }

    #[test]
    fn relative_boundaries() {
        let now = 1_000_000;
        assert_eq!(format_relative(now, now - 59), "just now");
        assert_eq!(format_relative(now, now - 60), "1m ago");
        assert_eq!(format_relative(now, now - 8 * 60), "8m ago");
        assert_eq!(format_relative(now, now + 41 * 60), "in 41m");
        assert_eq!(format_relative(now, now - 2 * 60 * 60), "2h ago");
        // 3 days back carries the absolute day.
        assert_eq!(
            format_relative(now, now - 3 * 24 * 60 * 60),
            format!("3d ({}) ago", format_utc_day(now - 3 * 24 * 60 * 60))
        );
    }

    #[test]
    fn counts_compact_to_two_figures() {
        assert_eq!(humanize_count(999), "999");
        assert_eq!(humanize_count(1_000), "1K");
        assert_eq!(humanize_count(845_000), "845K");
        assert_eq!(humanize_count(1_542_320_459), "1.54G");
    }
}
