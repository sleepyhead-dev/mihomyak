//! Update scheduling: fixed interval, cron expressions and "on start".
//!
//! Cron uses the classic 5 fields (`minute hour day-of-month month day-of-week`)
//! with lists, ranges, steps, month/day names and `@hourly/@daily/@weekly/@monthly`
//! aliases, evaluated in local time. Local time comes from libc `localtime_r`,
//! which honours `TZ` (e.g. `TZ=Europe/Moscow`) and DST without a tz library.

use anyhow::{Context, Result, bail};

/// Broken-down local time needed for matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub minute: u32,
    pub hour: u32,
    pub day: u32,
    /// 1–12
    pub month: u32,
    /// 0–6, Sunday = 0
    pub weekday: u32,
}

/// Local time of a unix timestamp according to the process time zone.
pub fn local_time(unix: u64) -> LocalTime {
    let tm = local_tm(unix);
    LocalTime {
        minute: tm.tm_min as u32,
        hour: tm.tm_hour as u32,
        day: tm.tm_mday as u32,
        month: tm.tm_mon as u32 + 1,
        weekday: tm.tm_wday as u32,
    }
}

/// Offset of local time from UTC at `unix`, e.g. `+03:00` (logged at start so a
/// missing `TZ` is obvious).
pub fn utc_offset(unix: u64) -> String {
    let offset = local_tm(unix).tm_gmtoff;
    let sign = if offset < 0 { '-' } else { '+' };
    let minutes = offset.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

// musl deprecates `time_t` ahead of its 64-bit transition; it is 64-bit on every
// target we build for.
#[allow(deprecated)]
fn local_tm(unix: u64) -> libc::tm {
    let t = unix as libc::time_t;
    // SAFETY: zeroed tm is a valid out-parameter; localtime_r is thread-safe.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    tm
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    source: String,
    minutes: u64,
    hours: u32,
    days: u32,
    months: u16,
    weekdays: u8,
    /// Neither day field starts with `*`: Vixie cron matches either of them
    /// (`*/2` counts as starred, as in Vixie cron).
    day_or: bool,
}

impl std::str::FromStr for Cron {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let expanded = match s.trim().to_ascii_lowercase().as_str() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            _ => s.trim(),
        };
        let fields: Vec<&str> = expanded.split_whitespace().collect();
        let [min, hour, dom, mon, dow] = fields[..] else {
            bail!("cron {s:?}: expected 5 fields (minute hour day month weekday)");
        };
        let ctx = |f: &str| format!("cron {s:?}: bad {f} field");
        let weekdays = field(dow, 0, 7, DAY_NAMES).with_context(|| ctx("weekday"))?;
        // 7 is Sunday too.
        let weekdays = (weekdays & 0x7f) | u64::from(weekdays & (1 << 7) != 0);
        let cron = Self {
            source: s.trim().to_owned(),
            minutes: field(min, 0, 59, &[]).with_context(|| ctx("minute"))?,
            hours: field(hour, 0, 23, &[]).with_context(|| ctx("hour"))? as u32,
            days: field(dom, 1, 31, &[]).with_context(|| ctx("day"))? as u32,
            months: field(mon, 1, 12, MONTH_NAMES).with_context(|| ctx("month"))? as u16,
            weekdays: weekdays as u8,
            day_or: !dom.starts_with('*') && !dow.starts_with('*'),
        };
        if !cron.can_match() {
            bail!("cron {s:?} never matches (no such day in the selected months)");
        }
        Ok(cron)
    }
}

const MONTH_NAMES: &[&str] = &[
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAY_NAMES: &[&str] = &["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

/// Parses one field into a bit set of allowed values.
fn field(spec: &str, min: u32, max: u32, names: &[&str]) -> Result<u64> {
    let value = |v: &str| -> Result<u32> {
        let lower = v.to_ascii_lowercase();
        if let Some(i) = names.iter().position(|n| *n == lower) {
            // Month names start at 1 (min = 1), day names at 0 (min = 0).
            return Ok(i as u32 + min);
        }
        let n: u32 = v
            .parse()
            .with_context(|| format!("{v:?} is not a number"))?;
        if !(min..=max).contains(&n) {
            bail!("{n} is outside {min}-{max}");
        }
        Ok(n)
    };
    let mut bits = 0u64;
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().context("bad step")?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step must be positive");
        }
        let (lo, hi) = match range {
            "*" => (min, max),
            r => match r.split_once('-') {
                Some((a, b)) => (value(a)?, value(b)?),
                None if part.contains('/') => (value(r)?, max),
                None => (value(r)?, value(r)?),
            },
        };
        if lo > hi {
            bail!("range {lo}-{hi} is reversed");
        }
        for v in (lo..=hi).step_by(step as usize) {
            bits |= 1 << v;
        }
    }
    Ok(bits)
}

impl Cron {
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether some calendar date satisfies the day-of-month/month fields
    /// (day-of-week alone always matches eventually).
    fn can_match(&self) -> bool {
        const DAYS_IN_MONTH: [u32; 12] = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
        if self.day_or {
            return true;
        }
        (1..=12u32).any(|month| {
            self.months & (1 << month) != 0
                && (1..=DAYS_IN_MONTH[month as usize - 1]).any(|day| self.days & (1 << day) != 0)
        })
    }

    pub fn matches(&self, t: &LocalTime) -> bool {
        let day = self.days & (1 << t.day) != 0;
        let weekday = self.weekdays & (1 << t.weekday) != 0;
        let day_ok = if self.day_or {
            day || weekday
        } else {
            day && weekday
        };
        self.minutes & (1 << t.minute) != 0
            && self.hours & (1 << t.hour) != 0
            && self.months & (1 << t.month) != 0
            && day_ok
    }

    /// First matching minute strictly after `after` (unix seconds), within four
    /// years (so `29 2` finds the next leap day).
    pub fn next_after(&self, after: u64, local: impl Fn(u64) -> LocalTime) -> Option<u64> {
        let mut t = (after / 60 + 1) * 60;
        let limit = t + 4 * 366 * 86_400;
        while t < limit {
            let lt = local(t);
            if self.hours & (1 << lt.hour) == 0 {
                // Jump to the next hour boundary.
                t += u64::from(60 - lt.minute) * 60;
                continue;
            }
            if self.matches(&lt) {
                return Some(t);
            }
            t += 60;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Local time for a fixed UTC offset (tests must not depend on the host TZ).
    fn at_offset(hours: i64) -> impl Fn(u64) -> LocalTime {
        move |unix| {
            let secs = unix as i64 + hours * 3600;
            let days = secs.div_euclid(86_400);
            let rem = secs.rem_euclid(86_400);
            let date = crate::util::fmt_date((days * 86_400) as u64);
            let mut parts = date.split('-').map(|p| p.parse::<u32>().unwrap());
            let (_, month, day) = (parts.next(), parts.next().unwrap(), parts.next().unwrap());
            LocalTime {
                minute: (rem % 3600 / 60) as u32,
                hour: (rem / 3600) as u32,
                day,
                month,
                // 1970-01-01 was a Thursday.
                weekday: ((days + 4).rem_euclid(7)) as u32,
            }
        }
    }

    const SEP_25_2026_2140Z: u64 = 1_790_372_400; // Friday

    #[test]
    fn daily_at_five_moscow() {
        let cron: Cron = "0 5 * * *".parse().unwrap();
        let next = cron.next_after(SEP_25_2026_2140Z, at_offset(3)).unwrap();
        // 05:00 MSK on the 26th = 02:00Z.
        assert_eq!(crate::util::fmt_timestamp(next), "2026-09-26T02:00:00Z");
    }

    #[test]
    fn steps_lists_names() {
        let cron: Cron = "*/15 9-18 * * mon-fri".parse().unwrap();
        let next = cron.next_after(SEP_25_2026_2140Z, at_offset(0)).unwrap();
        // Friday evening → Monday 09:00.
        assert_eq!(crate::util::fmt_timestamp(next), "2026-09-28T09:00:00Z");
        let cron: Cron = "30 4 1,15 * *".parse().unwrap();
        let next = cron.next_after(SEP_25_2026_2140Z, at_offset(0)).unwrap();
        assert_eq!(crate::util::fmt_timestamp(next), "2026-10-01T04:30:00Z");
        let cron: Cron = "0 0 * dec sun".parse().unwrap();
        assert!(cron.next_after(SEP_25_2026_2140Z, at_offset(0)).is_some());
    }

    #[test]
    fn vixie_day_or_semantics_and_sunday_seven() {
        let cron: Cron = "0 12 1 * 7".parse().unwrap();
        let next = cron.next_after(SEP_25_2026_2140Z, at_offset(0)).unwrap();
        // Sunday the 27th comes before the 1st.
        assert_eq!(crate::util::fmt_timestamp(next), "2026-09-27T12:00:00Z");
    }

    #[test]
    fn starred_step_keeps_and_semantics() {
        // Vixie cron: `*/2` counts as `*`, so this is "odd days that are Mondays".
        let cron: Cron = "0 0 */2 * mon".parse().unwrap();
        let next = cron.next_after(SEP_25_2026_2140Z, at_offset(0)).unwrap();
        let lt = at_offset(0)(next);
        assert_eq!((lt.weekday, lt.day % 2), (1, 1));
    }

    #[test]
    fn leap_day_and_impossible_dates() {
        let cron: Cron = "0 0 29 2 *".parse().unwrap();
        let next = cron.next_after(SEP_25_2026_2140Z, at_offset(0)).unwrap();
        assert_eq!(crate::util::fmt_timestamp(next), "2028-02-29T00:00:00Z");
        assert!("0 0 31 2 *".parse::<Cron>().is_err());
        assert!("0 0 31 4,6 *".parse::<Cron>().is_err());
        assert!(
            "0 0 31 2 mon".parse::<Cron>().is_ok(),
            "day-or makes it possible"
        );
    }

    #[test]
    fn aliases_and_errors() {
        assert_eq!(
            "@DAILY"
                .parse::<Cron>()
                .unwrap()
                .next_after(0, at_offset(0)),
            "@daily"
                .parse::<Cron>()
                .unwrap()
                .next_after(0, at_offset(0))
        );
        let daily: Cron = "@daily".parse().unwrap();
        let explicit: Cron = "0 0 * * *".parse().unwrap();
        let utc = at_offset(0);
        assert_eq!(
            daily.next_after(SEP_25_2026_2140Z, &utc),
            explicit.next_after(SEP_25_2026_2140Z, &utc)
        );
        for bad in [
            "",
            "* * * *",
            "60 * * * *",
            "* 24 * * *",
            "*/0 * * * *",
            "5-1 * * * *",
            "x * * * *",
        ] {
            assert!(bad.parse::<Cron>().is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn local_time_is_consistent() {
        let lt = local_time(SEP_25_2026_2140Z);
        assert!(lt.minute < 60 && lt.hour < 24 && (1..=12).contains(&lt.month));
        let offset = utc_offset(SEP_25_2026_2140Z);
        assert!(
            offset.len() == 6 && offset.starts_with(['+', '-']),
            "{offset}"
        );
    }
}
