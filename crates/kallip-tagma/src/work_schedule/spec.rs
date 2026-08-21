//! Native work-schedule spec: the structured form the UI edits and the
//! evaluator consumes. Replaces the retired cron-string representation.

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Minutes in a day; the exclusive upper bound for `end_minute`. A window
/// of `0..=DAY_MINUTES` covers a whole day.
pub const DAY_MINUTES: u16 = 24 * 60;

/// Most windows one weekly/monthly schedule may list. The cap is a
/// protective bound (each window is a day-of-day span); schedules
/// needing more segments are better expressed with a different mode.
pub const MAX_WINDOWS: usize = 10;

/// One duty span inside a weekly or monthly day: half-open
/// `[start_minute, end_minute)`; `end_minute <= start_minute` crosses
/// midnight and belongs to the start day (see [`Spec`]).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub start_minute: u16,
    pub end_minute: u16,
}

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
    Weekly { days: u8, windows: Vec<Window> },
    /// `days` is a bitmask: bit i = day-of-month i+1 (bit 0 = the 1st,
    /// bit 30 = the 31st). Days absent from a shorter month simply do
    /// not fire that month.
    Monthly { days: u32, windows: Vec<Window> },
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
            Spec::Weekly { days, windows } => {
                if *days == 0 {
                    return Err("weekly spec needs at least one day".into());
                }
                if *days > 0b0111_1111 {
                    return Err("weekly days bitmask uses bits 0-6 only".into());
                }
                validate_windows(windows)?;
            }
            Spec::Monthly { days, windows } => {
                if *days == 0 {
                    return Err("monthly spec needs at least one day".into());
                }
                if *days > (1u32 << 31) - 1 {
                    return Err("monthly days bitmask uses bits 0-30 only".into());
                }
                validate_windows(windows)?;
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

/// A window list is legal when every window is, the count stays within
/// [`MAX_WINDOWS`], and no two windows overlap on the absolute timeline
/// when laid out on the same day: each window spans
/// `[day + start, day + end)` with `end <= start` extended past midnight
/// (`22:00..02:00` vs `00:00..06:00` on one day overlaps at 00:00..02:00).
/// Touching endpoints (`end == start`) are fine — the evaluator merges
/// adjacent windows into one covering span.
fn validate_windows(windows: &[Window]) -> Result<(), String> {
    if windows.is_empty() {
        return Err("schedule needs at least one window".into());
    }
    if windows.len() > MAX_WINDOWS {
        return Err(format!("at most {MAX_WINDOWS} windows are allowed"));
    }
    for w in windows {
        validate_window(w.start_minute, w.end_minute)?;
    }
    // Absolute spans of each window laid on an arbitrary common day;
    // overnight windows extend to day + 1440 + end.
    // A span also collides with a next-day-anchored span (an overnight
    // tail meets the following day's window) in BOTH directions —
    // whichever window the pair holds first, so offsets +1440 and
    // −1440 are both checked. Deliberately mask-agnostic, which can
    // reject masks that never select consecutive days.
    let spans: Vec<(i32, i32)> = windows
        .iter()
        .map(|w| {
            let s = i32::from(w.start_minute);
            let e = i32::from(w.end_minute);
            (
                s,
                if e <= s {
                    e + i32::from(DAY_MINUTES)
                } else {
                    e
                },
            )
        })
        .collect();
    for i in 0..spans.len() {
        for j in (i + 1)..spans.len() {
            let (s1, e1) = spans[i];
            let (s2, e2) = spans[j];
            let overlaps = |off: i32| s1 < e2 - off && s2 - off < e1;
            if overlaps(0) || overlaps(-1440) || overlaps(1440) {
                return Err("windows overlap when laid on selected days".into());
            }
        }
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
                windows: vec![Window {
                    start_minute: 540,
                    end_minute: 1020,
                }],
            },
            Spec::Monthly {
                days: 1 | (1 << 30),
                windows: vec![Window {
                    start_minute: 0,
                    end_minute: DAY_MINUTES,
                }],
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
            windows: vec![Window {
                start_minute: 0,
                end_minute: 60,
            }],
        })
        .unwrap();
        assert!(json.contains(r#""mode":"weekly""#));
    }

    #[test]
    fn rejects_empty_weekly_days() {
        let spec = Spec::Weekly {
            days: 0,
            windows: vec![Window {
                start_minute: 0,
                end_minute: 60,
            }],
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_weekly_bit_above_sunday() {
        let spec = Spec::Weekly {
            days: 0b1000_0000,
            windows: vec![Window {
                start_minute: 0,
                end_minute: 60,
            }],
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_empty_monthly_days() {
        let spec = Spec::Monthly {
            days: 0,
            windows: vec![Window {
                start_minute: 0,
                end_minute: 60,
            }],
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_monthly_bit_for_day_32() {
        let spec = Spec::Monthly {
            days: 1 << 31,
            windows: vec![Window {
                start_minute: 0,
                end_minute: 60,
            }],
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_empty_window() {
        let spec = Spec::Weekly {
            days: 1,
            windows: vec![Window {
                start_minute: 540,
                end_minute: 540,
            }],
        };
        assert!(spec.validate().is_err());
    }

    #[test]
    fn accepts_full_day_and_overnight_windows() {
        let full = Spec::Weekly {
            days: 1,
            windows: vec![Window {
                start_minute: 0,
                end_minute: DAY_MINUTES,
            }],
        };
        assert!(full.validate().is_ok());
        // 22:00..06:00 crosses midnight and belongs to the start day.
        let overnight = Spec::Weekly {
            days: 1,
            windows: vec![Window {
                start_minute: 22 * 60,
                end_minute: 6 * 60,
            }],
        };
        assert!(overnight.validate().is_ok());
    }

    #[test]
    fn rejects_out_of_range_minutes() {
        let over = Spec::Weekly {
            days: 1,
            windows: vec![Window {
                start_minute: DAY_MINUTES,
                end_minute: DAY_MINUTES,
            }],
        };
        assert!(over.validate().is_err());
        let zero_end = Spec::Weekly {
            days: 1,
            windows: vec![Window {
                start_minute: 0,
                end_minute: 0,
            }],
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

    fn weekly(windows: Vec<Window>) -> Spec {
        Spec::Weekly {
            days: 0b0000_0001,
            windows,
        }
    }

    #[test]
    fn rejects_disjoint_minutes_that_overlap_absolute() {
        // 22:00..02:00 and 00:00..06:00 do not intersect as
        // minute ranges, but laid on one day they overlap at
        // 00:00..02:00.
        let spec = weekly(vec![
            Window {
                start_minute: 22 * 60,
                end_minute: 2 * 60,
            },
            Window {
                start_minute: 0,
                end_minute: 6 * 60,
            },
        ]);
        assert!(spec.validate().is_err());
    }
    #[test]
    fn rejects_disjoint_overlap_in_either_order() {
        // Same pair as the test above with the array order swapped: the
        // overnight tail of the late window meets the early window
        // anchored one day LATER, so both next-day directions must be
        // checked, not just the one this array order happens to hit.
        let spec = weekly(vec![
            Window {
                start_minute: 0,
                end_minute: 6 * 60,
            },
            Window {
                start_minute: 22 * 60,
                end_minute: 2 * 60,
            },
        ]);
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_overnight_tail_meeting_next_days_window() {
        // 00:00..02:00 and 23:00..01:00: same-day and span2-next-day
        // both read clean, but the 23:00 window on day d reaches into
        // day d+1 where the 00:00..02:00 window lives.
        let spec = weekly(vec![
            Window {
                start_minute: 0,
                end_minute: 2 * 60,
            },
            Window {
                start_minute: 23 * 60,
                end_minute: 1 * 60,
            },
        ]);
        assert!(spec.validate().is_err());
    }

    #[test]
    fn accepts_touching_windows() {
        // 09:00..12:00 then 12:00..17:00 share an endpoint; the
        // evaluator merges them into one covering span.
        let spec = weekly(vec![
            Window {
                start_minute: 9 * 60,
                end_minute: 12 * 60,
            },
            Window {
                start_minute: 12 * 60,
                end_minute: 17 * 60,
            },
        ]);
        assert!(spec.validate().is_ok());
    }

    #[test]
    fn rejects_plainly_overlapping_windows() {
        let spec = weekly(vec![
            Window {
                start_minute: 9 * 60,
                end_minute: 12 * 60,
            },
            Window {
                start_minute: 11 * 60,
                end_minute: 13 * 60,
            },
        ]);
        assert!(spec.validate().is_err());
    }

    #[test]
    fn rejects_window_count_above_the_cap() {
        let windows: Vec<Window> = (0..=MAX_WINDOWS)
            .map(|i| Window {
                start_minute: (i as u16) * 60,
                end_minute: (i as u16) * 60 + 30,
            })
            .collect();
        assert!(weekly(windows).validate().is_err());
    }

    #[test]
    fn accepts_exactly_capped_disjoint_windows() {
        let windows: Vec<Window> = (0..MAX_WINDOWS)
            .map(|i| Window {
                start_minute: (i as u16) * 90,
                end_minute: (i as u16) * 90 + 60,
            })
            .collect();
        assert!(weekly(windows).validate().is_ok());
    }

    #[test]
    fn rejects_empty_window_list() {
        assert!(weekly(vec![]).validate().is_err());
    }
}
