//! When a macro runs by itself: on chosen days at a time, once, or every few minutes in a span of the day.
//! Always in the computer's local time, to the minute.

use serde::{Deserialize, Serialize};

use crate::i18n::Msg;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Schedule {
    /// Off: kept, but the macro does not run by itself
    #[serde(default = "yes")]
    pub on: bool,
    #[serde(flatten)]
    pub when: When,
}

fn yes() -> bool {
    true
}

/// Days are 1 = Monday … 7 = Sunday; times "HH:MM".
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum When {
    Weekly { days: Vec<u8>, time: String },
    /// "YYYY-MM-DD HH:MM"
    Once { at: String },
    /// Every `minutes` from `from` to `to`; a span past midnight (22:00–02:00) is allowed
    Every { minutes: u32, from: String, to: String, days: Vec<u8> },
}

/// A minute of local time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Minute {
    pub y: i32,
    pub mo: u32,
    pub d: u32,
    pub h: u32,
    pub mi: u32,
}

impl Minute {
    pub fn now() -> Minute {
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        Minute { y: t.wYear.into(), mo: t.wMonth.into(), d: t.wDay.into(), h: t.wHour.into(), mi: t.wMinute.into() }
    }

    /// "YYYY-MM-DD HH:MM": sorts like the time itself.
    pub fn key(&self) -> String {
        format!("{} {:02}:{:02}", self.date(), self.h, self.mi)
    }

    pub fn date(&self) -> String {
        format!("{:04}-{:02}-{:02}", self.y, self.mo, self.d)
    }

    /// "YYYY-MM-DD HH:MM" (or with a T, as a phone's date-time field gives it).
    pub fn parse(s: &str) -> Option<Minute> {
        let s = s.trim();
        let (date, time) = s.split_once([' ', 'T'])?;
        let mut d = date.split('-');
        let (y, mo, day) = (d.next()?.parse().ok()?, d.next()?.parse().ok()?, d.next()?.parse().ok()?);
        let of_day = hm(time)?;
        let m = Minute { y, mo, d: day, h: of_day / 60, mi: of_day % 60 };
        (d.next().is_none() && (1..=12).contains(&mo) && day >= 1 && day <= days_in_month(y, mo) && (1970..=9999).contains(&y))
            .then_some(m)
    }

    fn days(&self) -> i64 {
        days_from_civil(self.y, self.mo, self.d)
    }

    /// 1 = Monday … 7 = Sunday
    pub fn weekday(&self) -> u8 {
        // Day 0 (1970-01-01) was a Thursday
        ((self.days() + 3).rem_euclid(7) + 1) as u8
    }

    fn of_day(&self) -> u32 {
        self.h * 60 + self.mi
    }

    pub fn plus(&self, minutes: i64) -> Minute {
        let total = self.days() * 1440 + self.of_day() as i64 + minutes;
        let (y, mo, d) = civil_from_days(total.div_euclid(1440));
        let m = total.rem_euclid(1440) as u32;
        Minute { y, mo, d, h: m / 60, mi: m % 60 }
    }
}

/// "HH:MM" as minutes of the day.
pub fn hm(s: &str) -> Option<u32> {
    let (h, m) = s.trim().split_once(':')?;
    let (h, m): (u32, u32) = (h.parse().ok()?, m.get(..2)?.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

fn days_in_month(y: i32, mo: u32) -> u32 {
    match mo {
        2 if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

// Days since 1970-01-01 and back (Howard Hinnant's civil date algorithms)
fn days_from_civil(y: i32, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y as i64 - 1 } else { y as i64 };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y as i32, m, d)
}

impl Schedule {
    /// Should the macro start at this minute?
    pub fn due(&self, now: &Minute) -> bool {
        if !self.on {
            return false;
        }
        match &self.when {
            When::Weekly { days, time } => days.contains(&now.weekday()) && hm(time) == Some(now.of_day()),
            When::Once { at } => Minute::parse(at).is_some_and(|m| m == *now),
            When::Every { minutes, from, to, days } => {
                let (Some(f), Some(t)) = (hm(from), hm(to)) else { return false };
                let m = now.of_day();
                let inside = if f <= t { (f..=t).contains(&m) } else { m >= f || m <= t };
                inside && days.contains(&now.weekday()) && ((m + 1440 - f) % 1440).is_multiple_of((*minutes).max(1))
            }
        }
    }

    /// The next minute after `now` it starts at, within a week and a day.
    pub fn next(&self, now: &Minute) -> Option<Minute> {
        if !self.on {
            return None;
        }
        if let When::Once { at } = &self.when {
            return Minute::parse(at).filter(|m| m.key() > now.key());
        }
        (1..=8 * 1440).map(|i| now.plus(i)).find(|m| self.due(m))
    }

    /// Checks the values and puts them in the stored form (days sorted, "HH:MM", a space in dates).
    pub fn normalize(&mut self) -> Result<(), Msg> {
        let bad = || Msg::new("err.mac_schedule");
        let tidy = |s: &mut String| -> Result<(), Msg> {
            let m = hm(s).ok_or_else(bad)?;
            *s = format!("{:02}:{:02}", m / 60, m % 60);
            Ok(())
        };
        let days_ok = |days: &mut Vec<u8>| {
            days.sort_unstable();
            days.dedup();
            !days.is_empty() && days.iter().all(|d| (1..=7).contains(d))
        };
        match &mut self.when {
            When::Weekly { days, time } => {
                tidy(time)?;
                if !days_ok(days) {
                    return Err(bad());
                }
            }
            When::Once { at } => *at = Minute::parse(at).ok_or_else(bad)?.key(),
            When::Every { minutes, from, to, days } => {
                tidy(from)?;
                tidy(to)?;
                if !days_ok(days) || !(1..=1440).contains(minutes) {
                    return Err(bad());
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> Minute {
        Minute::parse(s).unwrap()
    }

    #[test]
    fn calendar() {
        assert_eq!(at("2026-10-16 09:00").weekday(), 5);
        assert_eq!(at("1970-01-01 00:00").weekday(), 4);
        assert_eq!(at("2024-02-29 12:00").weekday(), 4);
        assert_eq!(at("2026-12-31 23:59").plus(1).key(), "2027-01-01 00:00");
        assert_eq!(at("2024-02-28 23:30").plus(60).key(), "2024-02-29 00:30");
        assert_eq!(at("2026-03-01 00:00").plus(-1).key(), "2026-02-28 23:59");
        assert_eq!(at("2026-10-16T14:05").key(), "2026-10-16 14:05");
        assert!(Minute::parse("2026-02-29 10:00").is_none());
        assert!(Minute::parse("2026-13-01 10:00").is_none());
        assert!(Minute::parse("2026-10-16 24:00").is_none());
        assert_eq!(hm("9:05"), Some(545));
        assert_eq!(hm("09:05:30"), Some(545));
        assert_eq!(hm("12:60"), None);
    }

    #[test]
    fn weekly() {
        let s = Schedule { on: true, when: When::Weekly { days: vec![1, 2, 3, 4, 5], time: "09:00".into() } };
        assert!(s.due(&at("2026-10-16 09:00")));
        assert!(!s.due(&at("2026-10-16 09:01")));
        assert!(!s.due(&at("2026-10-17 09:00"))); // Saturday
        assert_eq!(s.next(&at("2026-10-16 09:00")).unwrap().key(), "2026-10-19 09:00");
        assert_eq!(s.next(&at("2026-10-16 08:59")).unwrap().key(), "2026-10-16 09:00");
        let off = Schedule { on: false, ..s };
        assert!(!off.due(&at("2026-10-16 09:00")));
        assert!(off.next(&at("2026-10-16 08:00")).is_none());
    }

    #[test]
    fn every_few_minutes() {
        let days = vec![1, 2, 3, 4, 5, 6, 7];
        let s = Schedule { on: true, when: When::Every { minutes: 30, from: "09:00".into(), to: "18:00".into(), days } };
        assert!(s.due(&at("2026-10-16 09:00")));
        assert!(s.due(&at("2026-10-16 09:30")));
        assert!(s.due(&at("2026-10-16 18:00")));
        assert!(!s.due(&at("2026-10-16 09:15")));
        assert!(!s.due(&at("2026-10-16 18:30")));
        assert_eq!(s.next(&at("2026-10-16 18:00")).unwrap().key(), "2026-10-17 09:00");
        // Past midnight
        let night = Schedule {
            on: true,
            when: When::Every { minutes: 60, from: "22:00".into(), to: "02:00".into(), days: vec![1, 2, 3, 4, 5, 6, 7] },
        };
        assert!(night.due(&at("2026-10-16 23:00")));
        assert!(night.due(&at("2026-10-17 01:00")));
        assert!(!night.due(&at("2026-10-17 03:00")));
    }

    #[test]
    fn once() {
        let s = Schedule { on: true, when: When::Once { at: "2026-10-17 14:30".into() } };
        assert!(s.due(&at("2026-10-17 14:30")));
        assert!(!s.due(&at("2026-10-18 14:30")));
        assert_eq!(s.next(&at("2026-10-16 10:00")).unwrap().key(), "2026-10-17 14:30");
        assert!(s.next(&at("2026-10-17 14:30")).is_none());
    }

    #[test]
    fn stored_form() {
        let mut s = Schedule { on: true, when: When::Weekly { days: vec![5, 1, 5], time: "9:00".into() } };
        s.normalize().unwrap();
        assert_eq!(s.when, When::Weekly { days: vec![1, 5], time: "09:00".into() });
        let mut o = Schedule { on: true, when: When::Once { at: "2026-10-17T08:05".into() } };
        o.normalize().unwrap();
        assert_eq!(o.when, When::Once { at: "2026-10-17 08:05".into() });
        let mut none = Schedule { on: true, when: When::Weekly { days: vec![], time: "09:00".into() } };
        assert!(none.normalize().is_err());
        let mut eight = Schedule { on: true, when: When::Weekly { days: vec![8], time: "09:00".into() } };
        assert!(eight.normalize().is_err());
        let json = r#"{"on":true,"kind":"every","minutes":15,"from":"08:00","to":"12:00","days":[1]}"#;
        let e: Schedule = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&e).unwrap(), json);
    }
}
