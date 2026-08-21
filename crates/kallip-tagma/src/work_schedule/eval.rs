//! Evaluator for the native work-schedule spec: turns a [`Spec`] plus a
//! point in time into window facts. Replaces the retired cron evaluator;
//! unlike cron hour steps, the interval mode rotates strictly every N
//! hours across day boundaries.
//!
//! A display-only TypeScript port lives at
//! packages/kallip-ui/src/lib/manage/workSchedule.ts; any change to a
//! boundary, scan length, or window merge here must be mirrored there.

use time::{Date, Duration, OffsetDateTime, Time};

use crate::work_schedule::spec::{DAY_MINUTES, Spec, Window};

/// Window facts at a point in time.
///
/// `inside` is authoritative (computed directly from the spec), not
/// derived from boundary ordering — overlapping interval shifts
/// (`length >= period`, continuous duty) would break an order-based
/// derivation. When inside, `next_end` is the end of the covering
/// window(s) and `next_start` is the following shift's start; when
/// outside, both are the next future boundaries.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowStatus {
    pub inside: bool,
    pub next_start: OffsetDateTime,
    pub next_end: OffsetDateTime,
}

/// Evaluate `spec` at `now` (UTC).
pub fn window_status(spec: &Spec, now: OffsetDateTime) -> Option<WindowStatus> {
    match spec {
        Spec::Interval {
            every_hours,
            length_min,
            anchor,
        } => eval_interval(*every_hours, *length_min, *anchor, now),
        Spec::Weekly { days, windows } => eval_calendar(Some(*days), None, windows, now),
        Spec::Monthly { days, windows } => eval_calendar(None, Some(*days), windows, now),

        // 24/7: always inside. There is no natural end, so a finite
        // horizon (30 days) stands in — the engine wakes then and simply
        // re-evaluates to a fresh horizon, which also absorbs clock jumps.
        Spec::Always => Some(WindowStatus {
            inside: true,
            next_start: now + Duration::days(30),
            next_end: now + Duration::days(30),
        }),
    }
}

fn eval_interval(
    every_hours: u16,
    length_min: u16,
    anchor: OffsetDateTime,
    now: OffsetDateTime,
) -> Option<WindowStatus> {
    let period = Duration::hours(every_hours as i64);
    let length = Duration::minutes(length_min as i64);
    if now < anchor {
        // The rotation has not started yet: the anchor opens the first
        // shift. `next_end` is that first shift's end (a future fact),
        // even though `inside` is false.
        return Some(WindowStatus {
            inside: false,
            next_start: anchor,
            next_end: anchor + length,
        });
    }
    let elapsed = now - anchor;
    let period_secs = period.whole_seconds();
    let k = elapsed.whole_seconds().div_euclid(period_secs);
    let window_start = anchor + Duration::seconds(k * period_secs);
    let window_end = window_start + length;
    if window_start <= now && now < window_end {
        Some(WindowStatus {
            inside: true,
            next_start: window_start + period,
            next_end: window_end,
        })
    } else {
        let next = window_start + period;
        Some(WindowStatus {
            inside: false,
            next_start: next,
            next_end: next + length,
        })
    }
}

/// Shared evaluator for the day-mask modes (weekly ISO bitmask on
/// `week_days`, monthly day-of-month bitmask on `month_days`).
///
/// Windows are half-open `[start, end)`; a window whose `end` is on the
/// next day belongs to its start day. Adjacent windows that overlap are
/// one merged covering window (e.g. Mon 22:00..Tue 06:00 plus Tue
/// 04:00..08:00 covers through Tue 08:00). Candidate days are scanned
/// from yesterday (its overnight window may still cover `now`) through a
/// 70-day horizon — enough for the worst legal monthly gap (day 31
/// only, up to ~62 days across February).
fn eval_calendar(
    week_days: Option<u8>,
    month_days: Option<u32>,
    windows: &[Window],
    now: OffsetDateTime,
) -> Option<WindowStatus> {
    let today = now.date();
    // Absolute span of one window laid on day `d` (overnight windows
    // extend past midnight); None on a day the mask does not select.
    let span_on = |d: Date, w: &Window| -> Option<(OffsetDateTime, OffsetDateTime)> {
        let fires = match (week_days, month_days) {
            (Some(mask), _) => mask & (1 << (d.weekday().number_from_monday() - 1)) != 0,
            (_, Some(mask)) => mask & (1 << (d.day() - 1)) != 0,
            (None, None) => false,
        };
        if !fires {
            return None;
        }
        let s = i32::from(w.start_minute);
        let e = i32::from(w.end_minute);
        let e = if e <= s {
            e + i32::from(DAY_MINUTES)
        } else {
            e
        };
        let t = |m: i32| Time::from_hms((m / 60) as u8, (m % 60) as u8, 0).ok();
        let start = d.with_time(t(s)?).assume_utc();
        let end = d
            .checked_add(Duration::days(i64::from(e / i32::from(DAY_MINUTES))))?
            .with_time(t(e % i32::from(DAY_MINUTES))?)
            .assume_utc();
        Some((start, end))
    };
    // Earliest future window start, earliest future window end, and the
    // latest end among windows currently covering `now`.
    let mut next_start: Option<OffsetDateTime> = None;
    let mut next_end: Option<OffsetDateTime> = None;
    let mut covering_end: Option<OffsetDateTime> = None;
    for offset in -1..=70i64 {
        let d = today.checked_add(Duration::days(offset))?;
        for w in windows {
            let Some((ws, we)) = span_on(d, w) else {
                continue;
            };
            if ws <= now && now < we {
                covering_end = Some(match covering_end {
                    Some(e) if e >= we => e,
                    _ => we,
                });
            } else if ws > now {
                if next_start.map_or(true, |s| ws < s) {
                    next_start = Some(ws);
                }
                if next_end.map_or(true, |e| we < e) {
                    next_end = Some(we);
                }
            }
            // A window that already fully passed contributes nothing.
        }
    }
    if let Some(end) = covering_end {
        // A covering window always implies a later future window within
        // the horizon (the pattern repeats at least weekly/monthly);
        // `next_start` was collected alongside it.
        return Some(WindowStatus {
            inside: true,
            next_start: next_start?,
            next_end: end,
        });
    }
    Some(WindowStatus {
        inside: false,
        next_start: next_start?,
        next_end: next_end?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    fn ws(spec: &Spec, now: OffsetDateTime) -> WindowStatus {
        window_status(spec, now).unwrap()
    }

    #[test]
    fn weekly_inside_and_boundary_semantics() {
        // Mon 09:00-17:00, checked at Mon 12:00.
        let spec = Spec::Weekly {
            days: 0b0000_0001,
            windows: vec![Window {
                start_minute: 540,
                end_minute: 1020,
            }],
        };
        let st = ws(&spec, datetime!(2026-08-24 12:00 UTC)); // Monday
        assert!(st.inside);
        assert_eq!(st.next_end, datetime!(2026-08-24 17:00 UTC));
        assert_eq!(st.next_start, datetime!(2026-08-31 9:00 UTC));
        // Half-open: t == start is inside, t == end is outside.
        assert!(ws(&spec, datetime!(2026-08-24 9:00 UTC)).inside);
        assert!(!ws(&spec, datetime!(2026-08-24 17:00 UTC)).inside);
    }

    #[test]
    fn always_is_inside_at_every_instant() {
        let spec = Spec::Always;
        let st = ws(&spec, datetime!(2026-08-21 0:00 UTC));
        assert!(st.inside);
        // The horizon is a stand-in end, 30 days out; re-evaluation
        // at that point yields a fresh one.
        assert_eq!(
            st.next_end,
            datetime!(2026-08-21 0:00 UTC) + Duration::days(30)
        );
    }

    #[test]
    fn weekly_overnight_belongs_to_start_day() {
        // Mon 22:00..Tue 06:00.
        let spec = Spec::Weekly {
            days: 0b0000_0001,
            windows: vec![Window {
                start_minute: 22 * 60,
                end_minute: 6 * 60,
            }],
        };
        assert!(ws(&spec, datetime!(2026-08-24 23:00 UTC)).inside); // Mon night
        assert!(ws(&spec, datetime!(2026-08-25 5:59 UTC)).inside); // Tue small hours
        assert!(!ws(&spec, datetime!(2026-08-25 6:00 UTC)).inside); // window closed
        assert!(!ws(&spec, datetime!(2026-08-25 22:00 UTC)).inside); // Tue not selected
    }

    #[test]
    fn weekly_overnight_covers_midnight_boundary() {
        let spec = Spec::Weekly {
            days: 0b0000_0001,
            windows: vec![Window {
                start_minute: 22 * 60,
                end_minute: 6 * 60,
            }],
        };
        assert!(ws(&spec, datetime!(2026-08-25 0:00 UTC)).inside); // exactly midnight
    }

    #[test]
    fn weekly_all_days_means_daily() {
        let spec = Spec::Weekly {
            days: 0b0111_1111,
            windows: vec![Window {
                start_minute: 540,
                end_minute: 1020,
            }],
        };
        let st = ws(&spec, datetime!(2026-08-23 20:00 UTC)); // Sunday evening
        assert!(!st.inside);
        assert_eq!(st.next_start, datetime!(2026-08-24 9:00 UTC)); // Monday
    }

    #[test]
    fn monthly_skips_days_absent_from_short_months() {
        // Only day 31: Feb has none, April has one.
        let spec = Spec::Monthly {
            days: 1 << 30,
            windows: vec![Window {
                start_minute: 540,
                end_minute: 1020,
            }],
        };
        // Feb 28 2027 is a Sunday; next day-31 after that is Mar 31 2027.
        let st = ws(&spec, datetime!(2027-02-28 12:00 UTC));
        assert!(!st.inside);
        assert_eq!(st.next_start, datetime!(2027-03-31 9:00 UTC));
        // April 31 does not exist; after Apr 30 comes May 31.
        let st = ws(&spec, datetime!(2027-04-30 12:00 UTC));
        assert_eq!(st.next_start, datetime!(2027-05-31 9:00 UTC));
    }

    #[test]
    fn monthly_feb_29_leap_years() {
        // Only day 29: fires Feb 2028 (leap), skips Feb 2027.
        let spec = Spec::Monthly {
            days: 1 << 28,
            windows: vec![Window {
                start_minute: 0,
                end_minute: 60,
            }],
        };
        let st = ws(&spec, datetime!(2027-02-28 12:00 UTC));
        assert_eq!(st.next_start, datetime!(2027-03-29 0:00 UTC));
        let st = ws(&spec, datetime!(2028-02-28 12:00 UTC));
        assert_eq!(st.next_start, datetime!(2028-02-29 0:00 UTC));
    }

    #[test]
    fn monthly_cross_month_overnight_window() {
        // Jan 31 22:00..Feb 1 06:00.
        let spec = Spec::Monthly {
            days: 1 << 30,
            windows: vec![Window {
                start_minute: 22 * 60,
                end_minute: 6 * 60,
            }],
        };
        assert!(ws(&spec, datetime!(2026-01-31 23:00 UTC)).inside);
        assert!(ws(&spec, datetime!(2026-02-1 5:00 UTC)).inside);
        assert!(!ws(&spec, datetime!(2026-02-1 6:00 UTC)).inside);
    }

    #[test]
    fn calendar_full_day_window() {
        let spec = Spec::Weekly {
            days: 1,
            windows: vec![Window {
                start_minute: 0,
                end_minute: DAY_MINUTES,
            }],
        };
        assert!(ws(&spec, datetime!(2026-08-24 0:00 UTC)).inside);
        assert!(ws(&spec, datetime!(2026-08-24 23:59 UTC)).inside);
        assert!(!ws(&spec, datetime!(2026-08-25 0:00 UTC)).inside); // Tue not selected
    }

    #[test]
    fn adjacent_windows_merge_covering_end() {
        // Mon 22:00..Tue 06:00 and Tue 04:00..08:00 overlap on Tue
        // 04:00-06:00: at Tue 05:00 the covering end is Tue 08:00.
        let spec = Spec::Weekly {
            days: 0b0000_0011,
            windows: vec![Window {
                start_minute: 22 * 60,
                end_minute: 8 * 60,
            }],
        };
        let st = ws(&spec, datetime!(2026-08-25 5:00 UTC)); // Tue small hours
        assert!(st.inside);
        assert_eq!(st.next_end, datetime!(2026-08-25 8:00 UTC));
    }

    #[test]
    fn interval_strict_rotation_across_days() {
        // Every 5h from Aug 21 00:00, 90-minute shifts.
        let spec = Spec::Interval {
            every_hours: 5,
            length_min: 90,
            anchor: datetime!(2026-08-21 0:00 UTC),
        };
        assert!(ws(&spec, datetime!(2026-08-21 1:29 UTC)).inside);
        assert!(!ws(&spec, datetime!(2026-08-21 1:30 UTC)).inside);
        // Cross-day strictness: 20:00 start is 5h after 15:00, 01:00 next
        // day is 5h after 20:00 — no midnight re-alignment. The Aug 22
        // shift runs 01:00..02:30 (anchor + 25h), so 02:30 is out.
        assert!(ws(&spec, datetime!(2026-08-21 20:30 UTC)).inside);
        assert!(ws(&spec, datetime!(2026-08-22 1:29 UTC)).inside);
        assert!(!ws(&spec, datetime!(2026-08-22 2:30 UTC)).inside);
    }

    #[test]
    fn interval_before_anchor_waits_for_first_shift() {
        let spec = Spec::Interval {
            every_hours: 5,
            length_min: 60,
            anchor: datetime!(2026-08-21 0:00 UTC),
        };
        let st = ws(&spec, datetime!(2026-08-20 12:00 UTC));
        assert!(!st.inside);
        assert_eq!(st.next_start, datetime!(2026-08-21 0:00 UTC));
        assert_eq!(st.next_end, datetime!(2026-08-21 1:00 UTC));
    }

    #[test]
    fn interval_anchor_edges() {
        let spec = Spec::Interval {
            every_hours: 2,
            length_min: 60,
            anchor: datetime!(2026-08-21 0:00 UTC),
        };
        // Exactly at the anchor boundary: inside (half-open start).
        let st = ws(&spec, datetime!(2026-08-21 0:00 UTC));
        assert!(st.inside);
        assert_eq!(st.next_end, datetime!(2026-08-21 1:00 UTC));
        // Exactly at the window end: outside, next shift at 02:00.
        let st = ws(&spec, datetime!(2026-08-21 1:00 UTC));
        assert!(!st.inside);
        assert_eq!(st.next_start, datetime!(2026-08-21 2:00 UTC));
    }

    #[test]
    fn interval_length_ge_period_is_continuous_duty() {
        // 2h period, 3h length: shifts overlap — always inside.
        let spec = Spec::Interval {
            every_hours: 2,
            length_min: 180,
            anchor: datetime!(2026-08-21 0:00 UTC),
        };
        let st = ws(&spec, datetime!(2026-08-21 1:30 UTC));
        assert!(st.inside);
        // The covering window never ends; next_end reports the current
        // shift's nominal end (an implementation fact, not a duty break).
        assert_eq!(st.next_end, datetime!(2026-08-21 3:00 UTC));
    }

    #[test]
    fn weekly_next_after_far_gap() {
        // Sunday-only 09:00-10:00, checked Monday 12:00: next is 6 days out.
        let spec = Spec::Weekly {
            days: 0b0100_0000,
            windows: vec![Window {
                start_minute: 540,
                end_minute: 600,
            }],
        };
        let st = ws(&spec, datetime!(2026-08-24 12:00 UTC)); // Monday
        assert!(!st.inside);
        assert_eq!(st.next_start, datetime!(2026-08-30 9:00 UTC)); // Sunday
    }

    #[test]
    fn two_disjoint_windows_one_day() {
        // Mon 09:00..12:00 and 13:00..17:00.
        let spec = Spec::Weekly {
            days: 0b0000_0001,
            windows: vec![
                Window {
                    start_minute: 9 * 60,
                    end_minute: 12 * 60,
                },
                Window {
                    start_minute: 13 * 60,
                    end_minute: 17 * 60,
                },
            ],
        };
        // In the morning window: ends at the first window's end.
        let st = ws(&spec, datetime!(2026-08-24 10:00 UTC));
        assert!(st.inside);
        assert_eq!(st.next_end, datetime!(2026-08-24 12:00 UTC));
        // In the midday gap: next start is the afternoon window.
        let st = ws(&spec, datetime!(2026-08-24 12:30 UTC));
        assert!(!st.inside);
        assert_eq!(st.next_start, datetime!(2026-08-24 13:00 UTC));
        // In the afternoon window, next duty is next Monday.
        let st = ws(&spec, datetime!(2026-08-24 16:00 UTC));
        assert_eq!(st.next_start, datetime!(2026-08-31 9:00 UTC));
    }
}
