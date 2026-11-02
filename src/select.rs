//! Selectors: which plan files a command acts on.
//!
//! A selector names either **one day** or **a set of existing plans**:
//!
//! | selector                          | meaning                                  |
//! |-----------------------------------|------------------------------------------|
//! | `@`, `today`                      | today                                    |
//! | `yesterday`, `@~N`, `N days ago`  | N days before today                      |
//! | `@-`                              | the most recent existing plan            |
//! | `2026-11-01`                      | that date                                |
//! | `11-01`                           | every plan on Nov 1, in any year         |
//! | `01`, `1`                         | every plan on the 1st, in any month      |
//! | `2026-11`                         | every plan in that month                 |
//! | `2026`                            | every plan in that year                  |
//! | `A..B`, `A..`, `..B`, `..`        | every plan from A through B, inclusive   |
//!
//! Range bounds are days, months, or years; a month or year bound covers the
//! whole month or year. A day can name a plan that doesn't exist yet (today's
//! is created on demand); a set only ever contains plans that exist.
//!
//! Everything here is pure: no clock, no filesystem. Callers pass in today and
//! the dates of the plans that exist.

use jiff::civil::Date;
use jiff::{Span, ToSpan};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Day(Day),
    Set(Set),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Day {
    /// N days before today.
    Ago(u32),
    On(Date),
    /// The most recent existing plan.
    Latest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
    Range {
        from: Option<Bound>,
        to: Option<Bound>,
    },
    Year(i16),
    Month(i16, i8),
    MonthDay(i8, i8),
    DayOfMonth(i8),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bound {
    Day(Day),
    Month(i16, i8),
    Year(i16),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    Invalid(String),
    /// The date is outside the representable range.
    OutOfRange,
    /// `@-` with no plans at all.
    NoPlans,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Invalid(msg) => f.write_str(msg),
            Error::OutOfRange => {
                f.write_str("Date calculation is out of bounds (too far in the past).")
            }
            Error::NoPlans => f.write_str("There are no plans yet."),
        }
    }
}

impl std::error::Error for Error {}

/// What a selector can be.
pub const USAGE: &str = "Use a day (@, @~N, @-, today, yesterday, 'N days ago', 2026-11-01), \
     a set of plans (11-01, 01, 2026-11, 2026), or a range (@~6.., 2026-10..2026-11, ..).";

pub const HELP: &str = "Invalid selector.";

/// Parse a selector.
pub fn parse(s: &str) -> Result<Selector, Error> {
    let s = s.trim();
    if let Some((from, to)) = s.split_once("..") {
        if to.contains("..") {
            return Err(Error::Invalid(format!("{s:?} has more than one `..`.")));
        }
        return Ok(Selector::Set(Set::Range {
            from: parse_bound(from)?,
            to: parse_bound(to)?,
        }));
    }
    parse_one(s)
}

fn parse_bound(s: &str) -> Result<Option<Bound>, Error> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    match parse_one(s)? {
        Selector::Day(day) => Ok(Some(Bound::Day(day))),
        Selector::Set(Set::Month(y, m)) => Ok(Some(Bound::Month(y, m))),
        Selector::Set(Set::Year(y)) => Ok(Some(Bound::Year(y))),
        Selector::Set(_) => Err(Error::Invalid(format!(
            "{s:?} can't bound a range: use a day, a month, or a year."
        ))),
    }
}

fn parse_one(s: &str) -> Result<Selector, Error> {
    if s == "@-" {
        return Ok(Selector::Day(Day::Latest));
    }
    if let Some(parts) = digit_groups(s) {
        return parse_numeric(s, &parts);
    }
    crate::date::parse_date_opt(Some(s))
        .map(|n| Selector::Day(Day::Ago(n)))
        .map_err(|e| {
            let msg = e.to_string();
            // The generic message predates sets and ranges; say what's possible now.
            if msg.starts_with("Invalid date format. Use") {
                Error::Invalid(format!("{HELP} {USAGE}"))
            } else {
                Error::Invalid(msg)
            }
        })
}

/// `s` split on `-`, if it is only digit groups (`2026-11-01`, `11`, ...).
fn digit_groups(s: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = s.split('-').collect();
    parts
        .iter()
        .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        .then_some(parts)
}

fn parse_numeric(s: &str, parts: &[&str]) -> Result<Selector, Error> {
    let invalid = || Error::Invalid(format!("{s:?} is not a valid date. {USAGE}"));
    let num = |p: &str| p.parse::<i64>().map_err(|_| invalid());
    let lens: Vec<usize> = parts.iter().map(|p| p.len()).collect();
    match lens[..] {
        [4, 2, 2] => {
            let date = Date::new(
                num(parts[0])? as i16,
                num(parts[1])? as i8,
                num(parts[2])? as i8,
            )
            .map_err(|_| invalid())?;
            Ok(Selector::Day(Day::On(date)))
        }
        [4, 1..=2] => {
            let (y, m) = (num(parts[0])? as i16, num(parts[1])?);
            if !(1..=12).contains(&m) {
                return Err(invalid());
            }
            Date::new(y, m as i8, 1).map_err(|_| invalid())?;
            Ok(Selector::Set(Set::Month(y, m as i8)))
        }
        [4] => {
            let y = num(parts[0])? as i16;
            Date::new(y, 1, 1).map_err(|_| invalid())?;
            Ok(Selector::Set(Set::Year(y)))
        }
        [1..=2, 1..=2] => {
            let (m, d) = (num(parts[0])?, num(parts[1])?);
            // 2024 is a leap year, so 02-29 is a real month-day.
            if !(1..=12).contains(&m) || Date::new(2024, m as i8, d as i8).is_err() {
                return Err(invalid());
            }
            Ok(Selector::Set(Set::MonthDay(m as i8, d as i8)))
        }
        [1..=2] => {
            let d = num(parts[0])?;
            if !(1..=31).contains(&d) {
                return Err(invalid());
            }
            Ok(Selector::Set(Set::DayOfMonth(d as i8)))
        }
        _ => Err(invalid()),
    }
}

/// What a selector picked out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// One day, whose plan may or may not exist.
    Day(Date),
    /// Existing plans, newest first.
    Plans(Vec<Date>),
}

/// Evaluate `selector` against `today` and the dates of existing `plans`.
pub fn resolve(selector: &Selector, today: Date, plans: &[Date]) -> Result<Resolved, Error> {
    match selector {
        Selector::Day(day) => resolve_day(*day, today, plans).map(Resolved::Day),
        Selector::Set(set) => {
            let (lo, hi) = match *set {
                Set::Range { from, to } => {
                    let lo = match from {
                        Some(b) => lower(b, today, plans)?,
                        None => Some(Date::MIN),
                    };
                    let hi = match to {
                        Some(b) => upper(b, today, plans)?,
                        None => Some(Date::MAX),
                    };
                    (lo, hi)
                }
                _ => (Some(Date::MIN), Some(Date::MAX)),
            };
            let mut matches: Vec<Date> = match (lo, hi) {
                (Some(lo), Some(hi)) => plans
                    .iter()
                    .copied()
                    .filter(|d| lo <= *d && *d <= hi && in_set(set, *d))
                    .collect(),
                // A bound of `@-` when there are no plans: nothing can match.
                _ => Vec::new(),
            };
            matches.sort_unstable_by(|a, b| b.cmp(a));
            matches.dedup();
            Ok(Resolved::Plans(matches))
        }
    }
}

fn resolve_day(day: Day, today: Date, plans: &[Date]) -> Result<Date, Error> {
    match day {
        Day::Ago(n) => {
            // `try_days`, not `days`: huge offsets must be an error, not a panic.
            let span = Span::new().try_days(n).map_err(|_| Error::OutOfRange)?;
            today.checked_sub(span).map_err(|_| Error::OutOfRange)
        }
        Day::On(date) => Ok(date),
        Day::Latest => plans.iter().max().copied().ok_or(Error::NoPlans),
    }
}

/// The first day a bound covers; `None` if it names nothing (`@-`, no plans).
fn lower(bound: Bound, today: Date, plans: &[Date]) -> Result<Option<Date>, Error> {
    Ok(Some(match bound {
        Bound::Day(Day::Latest) if plans.is_empty() => return Ok(None),
        Bound::Day(day) => resolve_day(day, today, plans)?,
        Bound::Month(y, m) => Date::new(y, m, 1).map_err(|_| Error::OutOfRange)?,
        Bound::Year(y) => Date::new(y, 1, 1).map_err(|_| Error::OutOfRange)?,
    }))
}

/// The last day a bound covers.
fn upper(bound: Bound, today: Date, plans: &[Date]) -> Result<Option<Date>, Error> {
    Ok(Some(match bound {
        Bound::Day(Day::Latest) if plans.is_empty() => return Ok(None),
        Bound::Day(day) => resolve_day(day, today, plans)?,
        Bound::Month(y, m) => Date::new(y, m, 1)
            .map_err(|_| Error::OutOfRange)?
            .last_of_month(),
        Bound::Year(y) => Date::new(y, 12, 31).map_err(|_| Error::OutOfRange)?,
    }))
}

fn in_set(set: &Set, d: Date) -> bool {
    match *set {
        Set::Range { .. } => true,
        Set::Year(y) => d.year() == y,
        Set::Month(y, m) => d.year() == y && d.month() == m,
        Set::MonthDay(m, day) => d.month() == m && d.day() == day,
        Set::DayOfMonth(day) => d.day() == day,
    }
}

/// The selector for the page of `set` that comes before `oldest_shown`, when
/// `set` can be continued as a range (ranges, months, and years can).
pub fn earlier(set: &Set, today: Date, plans: &[Date], oldest_shown: Date) -> Option<String> {
    let lo = match *set {
        Set::Range { from: None, .. } => None,
        Set::Range { from: Some(b), .. } => Some(lower(b, today, plans).ok()??),
        Set::Year(y) => Some(Date::new(y, 1, 1).ok()?),
        Set::Month(y, m) => Some(Date::new(y, m, 1).ok()?),
        Set::MonthDay(..) | Set::DayOfMonth(_) => return None,
    };
    let hi = oldest_shown.checked_sub(1.day()).ok()?;
    let lo = lo
        .map(|d| d.strftime("%Y-%m-%d").to_string())
        .unwrap_or_default();
    Some(format!("{lo}..{}", hi.strftime("%Y-%m-%d")))
}

/// How much of a date it takes to pick out exactly one plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unique {
    /// `01`: no other plan falls on that day of the month.
    Day,
    /// `11-01`: no other plan falls on that month and day.
    MonthDay,
    /// `2026-11-01`
    Full,
}

impl Unique {
    /// The selector text for `date`: always a suffix of its `YYYY-MM-DD`.
    pub fn text(self, date: Date) -> String {
        let format = match self {
            Unique::Day => "%d",
            Unique::MonthDay => "%m-%d",
            Unique::Full => "%Y-%m-%d",
        };
        date.strftime(format).to_string()
    }
}

/// The shortest selector that matches `date` and no other plan in `plans`.
pub fn shortest_unique(date: Date, plans: &[Date]) -> Unique {
    let others = |same: &dyn Fn(&Date) -> bool| plans.iter().any(|p| *p != date && same(p));
    if !others(&|p| p.day() == date.day()) {
        Unique::Day
    } else if !others(&|p| p.month() == date.month() && p.day() == date.day()) {
        Unique::MonthDay
    } else {
        Unique::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    const TODAY: Date = date(2026, 2, 19);

    fn plans(days: &[(i16, i8, i8)]) -> Vec<Date> {
        days.iter().map(|&(y, m, d)| date(y, m, d)).collect()
    }

    fn picks(s: &str, plans: &[Date]) -> Vec<Date> {
        match resolve(&parse(s).unwrap(), TODAY, plans).unwrap() {
            Resolved::Plans(v) => v,
            Resolved::Day(d) => vec![d],
        }
    }

    #[test]
    fn parses_days() {
        assert_eq!(parse("@"), Ok(Selector::Day(Day::Ago(0))));
        assert_eq!(parse("today"), Ok(Selector::Day(Day::Ago(0))));
        assert_eq!(parse("yesterday"), Ok(Selector::Day(Day::Ago(1))));
        assert_eq!(parse("@~3"), Ok(Selector::Day(Day::Ago(3))));
        assert_eq!(parse("3 days ago"), Ok(Selector::Day(Day::Ago(3))));
        assert_eq!(parse("@-"), Ok(Selector::Day(Day::Latest)));
        assert_eq!(
            parse("2026-11-01"),
            Ok(Selector::Day(Day::On(date(2026, 11, 1))))
        );
    }

    #[test]
    fn parses_sets_and_ranges() {
        assert_eq!(parse("11-01"), Ok(Selector::Set(Set::MonthDay(11, 1))));
        assert_eq!(parse("01"), Ok(Selector::Set(Set::DayOfMonth(1))));
        assert_eq!(parse("7"), Ok(Selector::Set(Set::DayOfMonth(7))));
        assert_eq!(parse("2026-11"), Ok(Selector::Set(Set::Month(2026, 11))));
        assert_eq!(parse("2026"), Ok(Selector::Set(Set::Year(2026))));
        assert_eq!(
            parse(".."),
            Ok(Selector::Set(Set::Range {
                from: None,
                to: None
            }))
        );
        assert_eq!(
            parse("@~6.."),
            Ok(Selector::Set(Set::Range {
                from: Some(Bound::Day(Day::Ago(6))),
                to: None
            }))
        );
        assert_eq!(
            parse("2026-10..@-"),
            Ok(Selector::Set(Set::Range {
                from: Some(Bound::Month(2026, 10)),
                to: Some(Bound::Day(Day::Latest))
            }))
        );
    }

    #[test]
    fn rejects_nonsense() {
        for s in [
            "",
            "13-01",
            "02-30",
            "00",
            "32",
            "2026-13",
            "2026-02-30",
            "123",
            "1-2-3",
            "a..b",
            "11-01..",
            "..01",
            "1..2..3",
            "-1",
            "@~x",
        ] {
            assert!(parse(s).is_err(), "{s:?} should not parse: {:?}", parse(s));
        }
        assert!(parse("02-29").is_ok(), "leap day is a real month-day");
    }

    #[test]
    fn resolves_sets_newest_first() {
        let p = plans(&[(2025, 11, 1), (2026, 1, 15), (2026, 2, 1), (2026, 2, 18)]);
        assert_eq!(
            picks("..", &p),
            plans(&[(2026, 2, 18), (2026, 2, 1), (2026, 1, 15), (2025, 11, 1)])
        );
        assert_eq!(picks("01", &p), plans(&[(2026, 2, 1), (2025, 11, 1)]));
        assert_eq!(picks("11-01", &p), plans(&[(2025, 11, 1)]));
        assert_eq!(picks("2026-02", &p), plans(&[(2026, 2, 18), (2026, 2, 1)]));
        assert_eq!(picks("2025", &p), plans(&[(2025, 11, 1)]));
        assert_eq!(picks("@~18..", &p), plans(&[(2026, 2, 18), (2026, 2, 1)]));
        assert_eq!(
            picks("..2026-01", &p),
            plans(&[(2026, 1, 15), (2025, 11, 1)])
        );
        assert_eq!(picks("2026-01..2026-01-31", &p), plans(&[(2026, 1, 15)]));
        assert_eq!(picks("@-..", &p), plans(&[(2026, 2, 18)]));
        assert_eq!(picks("@-..", &[]), plans(&[]));
        assert_eq!(
            picks("@..@~30", &p),
            plans(&[]),
            "a backwards range is empty"
        );
    }

    #[test]
    fn resolves_days_whether_or_not_they_exist() {
        let p = plans(&[(2026, 2, 1)]);
        assert_eq!(picks("@", &p), plans(&[(2026, 2, 19)]));
        assert_eq!(picks("2026-02-10", &p), plans(&[(2026, 2, 10)]));
        assert_eq!(picks("@-", &p), plans(&[(2026, 2, 1)]));
        assert_eq!(
            resolve(&parse("@-").unwrap(), TODAY, &[]),
            Err(Error::NoPlans)
        );
        assert_eq!(
            resolve(&parse("@~4294967295").unwrap(), TODAY, &p),
            Err(Error::OutOfRange)
        );
    }

    #[test]
    fn earlier_continues_ranges_months_and_years() {
        let p = plans(&[(2026, 2, 1), (2026, 2, 18)]);
        let set = |s: &str| match parse(s).unwrap() {
            Selector::Set(set) => set,
            other => panic!("{other:?}"),
        };
        let oldest = date(2026, 2, 10);
        assert_eq!(
            earlier(&set(".."), TODAY, &p, oldest),
            Some("..2026-02-09".into())
        );
        assert_eq!(
            earlier(&set("@~30.."), TODAY, &p, oldest),
            Some("2026-01-20..2026-02-09".into())
        );
        assert_eq!(
            earlier(&set("2026-02"), TODAY, &p, oldest),
            Some("2026-02-01..2026-02-09".into())
        );
        assert_eq!(earlier(&set("01"), TODAY, &p, oldest), None);
    }

    #[test]
    fn shortest_unique_widens_as_needed() {
        let p = plans(&[(2025, 11, 1), (2026, 11, 1), (2026, 10, 1), (2026, 10, 28)]);
        assert_eq!(shortest_unique(date(2026, 10, 28), &p), Unique::Day);
        assert_eq!(shortest_unique(date(2026, 10, 1), &p), Unique::MonthDay);
        assert_eq!(shortest_unique(date(2026, 11, 1), &p), Unique::Full);
        assert_eq!(Unique::Day.text(date(2026, 10, 8)), "08");
    }
}
