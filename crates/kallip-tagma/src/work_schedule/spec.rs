//! Native work-schedule spec: the structured form the UI edits and the
//! evaluator consumes. Replaces the retired cron-string representation.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Minutes in a day; the exclusive upper bound for `end_minute`. A window
/// of `0..=DAY_MINUTES` covers a whole day.
pub const DAY_MINUTES: u16 = 24 * 60;

/// Longest allowed shift (7 days). Shifts at or above the repeat period
/// mean continuous duty anyway; beyond a week, that intent is better
/// expressed by not scheduling off-time at all.
pub const MAX_LENGTH_MINUTES: u16 = 7 * 24 * 60;

/// A single tagma-wide work schedule in structured form.
///
/// All times are UTC. Weekly/monthly windows are minute-of-day values;
/// `end_minute == DAY_MINUTES` means "to the end of the day" (full-day
/// shift). `end_minute < start_minute` denotes a window that crosses
/// midnight and belongs to its start day (Mon 22:00..Tue 06:00 fires on
/// Monday).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Spec {
    /// `days` is a bitmask: bit i = ISO weekday i+1 (bit 0 = Monday,
    /// bit 6 = Sunday).
    Weekly {
        days: u8,
        start_minute: u16,
        end_minute: u16,
    },
    /// `days` is a bitmask: bit i = day-of-month i+1 (bit 0 = the 1st,
    /// bit 30 = the 31st). Days absent from a shorter month simply do
    /// not fire that month.
    Monthly {
        days: u32,
        start_minute: u16,
        end_minute: u16,
    },
    /// Fixed-period rotation anchored at `anchor`: on duty for
    /// `length_min` starting at each `anchor + k * every_hours` (k >= 0).
    /// The rotation runs continuously across day boundaries — unlike a
    /// cron hour step there is no midnight re-alignment.
    Interval {
        every_hours: u16,
        length_min: u16,
        #[serde(with = "time::serde::rfc3339")]
        anchor: OffsetDateTime,
    },

    /// Always on duty — the 24/7 schedule as a first-class variant.
    /// Phase-free: unlike a weekly full-day mask it carries no day or
    /// minute fields, so it cannot drift or be misread under any clock.
    Always,
}

impl Spec {
    /// Validate the spec's numeric ranges and bitmasks.
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Spec::Weekly {
                days,
                start_minute,
                end_minute,
            } => {
                if *days == 0 {
                    return Err("weekly spec needs at least one day".into());
                }
                if *days > 0b0111_1111 {
                    return Err("weekly days bitmask uses bits 0-6 only".into());
                }
                validate_window(*start_minute, *end_minute)?;
            }
            Spec::Monthly {
                days,
                start_minute,
                end_minute,
            } => {
                if *days == 0 {
                    return Err("monthly spec needs at least one day".into());
                }
                if *days > (1u32 << 31) - 1 {
                    return Err("monthly days bitmask uses bits 0-30 only".into());
                }
                validate_window(*start_minute, *end_minute)?;
            }
            Spec::Interval {
                every_hours,
                length_min,
                anchor,
            } => {
                if *every_hours == 0 {
                    return Err("every_hours must be >= 1".into());
                }
                if *length_min == 0 {
                    return Err("length_min must be >= 1".into());
                }
                if *length_min > MAX_LENGTH_MINUTES {
                    return Err(format!(
                        "length_min must be <= {MAX_LENGTH_MINUTES} (7 days)"
                    ));
                }
                if anchor.second() != 0 || anchor.nanosecond() != 0 {
                    return Err("anchor must be minute-aligned".into());
                }
            }
            Spec::Always => {}
        }
        Ok(())
    }
}

/// A day-window is legal when both ends fall in `0..=DAY_MINUTES` and the
/// window is non-empty (`end == start` would never open).
fn validate_window(start_minute: u16, end_minute: u16) -> Result<(), String> {
    if start_minute >= DAY_MINUTES {
        return Err(format!("start_minute must be < {DAY_MINUTES}"));
    }
    if end_minute == 0 || end_minute > DAY_MINUTES {
        return Err(format!("end_minute must be in 1..={DAY_MINUTES}"));
    }
    if end_minute == start_minute {
        return Err("end_minute == start_minute yields an empty window".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn anchor() -> OffsetDateTime {
        datetime!(2026-08-21 0:00 UTC)
    }

    #[test]
    fn round_trips_all_modes_through_json() {
        let specs = vec![
            Spec::Weekly {
                days: 0b0010_0001,
                start_minute: 540,
                end_minute: 1020,
            },
            Spec::Monthly {
                days: 1 | (1 << 30),
                start_minute: 0,
                end_minute: DAY_MINUTES,
            },
            Spec::Interval {
                every_hours: 5,
                length_min: 90,
                anchor: anchor(),
            },
            Spec::Always,
        ];
        for spec in specs {
            let json = serde_json::to_string(&spec).unwrap();
            let back: Spec = serde_json::from_str(&json).unwrap();
            assert_eq!(back, spec);
        }
    }

    #[test]
    fn json_tags_use_snake_case_mode() {
        let json = serde_json::to_string(&Spec::Weekly {
            days: 1,
            start_minute: 0,
            end_minute: 60,
        })
        .unwrap();
        assert!(json.contains(r#""mode":"weekly""#));
    }

    #[test]
    fn rejects_empty_weekly_days() {
        let spec = Spec::Weekly {
            days: 0,
            start_minute: 0,
            end_minute: 60,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_weekly_bit_above_sunday() {
        let spec = Spec::Weekly {
            days: 0b1000_0000,
            start_minute: 0,
            end_minute: 60,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_empty_monthly_days() {
        let spec = Spec::Monthly {
            days: 0,
            start_minute: 0,
            end_minute: 60,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_monthly_bit_for_day_32() {
        let spec = Spec::Monthly {
            days: 1 << 31,
            start_minute: 0,
            end_minute: 60,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_empty_window() {
        let spec = Spec::Weekly {
            days: 1,
            start_minute: 540,
            end_minute: 540,
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn accepts_full_day_and_overnight_windows() {
        let full = Spec::Weekly {
            days: 1,
            start_minute: 0,
            end_minute: DAY_MINUTES,
        };
        assert!(full.validate().is_ok());
        // 22:00..06:00 crosses midnight and belongs to the start day.
        let overnight = Spec::Weekly {
            days: 1,
            start_minute: 22 * 60,
            end_minute: 6 * 60,
        };
        assert!(overnight.validate().is_ok());
    }

    #[test]
    fn rejects_out_of_range_minutes() {
        let over = Spec::Weekly {
            days: 1,
            start_minute: DAY_MINUTES,
            end_minute: DAY_MINUTES,
        };
        assert!(over.validate().is_err());
        let zero_end = Spec::Weekly {
            days: 1,
            start_minute: 0,
            end_minute: 0,
        };
        assert!(zero_end.validate().is_err());
    }

    #[test]
    fn rejects_zero_interval_fields() {
        let zero_every = Spec::Interval {
            every_hours: 0,
            length_min: 60,
            anchor: anchor(),
        };
        assert!(zero_every.validate().is_err());
        let zero_len = Spec::Interval {
            every_hours: 5,
            length_min: 0,
            anchor: anchor(),
        };
        assert!(zero_len.validate().is_err());
    }

    #[test]
    fn rejects_oversized_interval_length() {
        let spec = Spec::Interval {
            every_hours: 1,
            length_min: MAX_LENGTH_MINUTES + 1,
            anchor: anchor(),
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_sub_minute_anchor() {
        let spec = Spec::Interval {
            every_hours: 5,
            length_min: 60,
            anchor: datetime!(2026-08-21 0:00:30 UTC),
        };
        assert!(spec.validate().is_err());
    }
}
