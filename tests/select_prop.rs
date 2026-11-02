//! Property tests for selectors: each one is a promise the interface makes.

use hegel::TestCase;
use hegel::generators as gs;
use jiff::ToSpan;
use jiff::civil::{Date, date};
use plan::select::{self, Resolved, Selector};

const BASE: Date = date(2020, 1, 1);
const TODAY: Date = date(2026, 2, 19);

fn day(offset: i64) -> Date {
    BASE.checked_add(offset.days()).unwrap()
}

fn fmt(d: Date) -> String {
    d.strftime("%Y-%m-%d").to_string()
}

/// A set of existing plans spread over about eight years, so day-of-month and
/// month-day collisions are common.
fn draw_plans(tc: &TestCase) -> Vec<Date> {
    let offsets = tc.draw(
        gs::btree_sets(gs::integers::<i64>().min_value(0).max_value(3000))
            .min_size(1)
            .max_size(60),
    );
    offsets.into_iter().map(day).collect()
}

fn picks(selector: &str, plans: &[Date]) -> Vec<Date> {
    let parsed = select::parse(selector).unwrap_or_else(|e| panic!("{selector:?}: {e}"));
    match select::resolve(&parsed, TODAY, plans).unwrap() {
        Resolved::Plans(v) => v,
        Resolved::Day(d) => vec![d],
    }
}

fn newest_first(mut v: Vec<Date>) -> Vec<Date> {
    v.sort_unstable_by(|a, b| b.cmp(a));
    v
}

// What the list view highlights is a selector for exactly that plan.
#[hegel::test(test_cases = 256, database = None)]
fn highlighted_suffix_picks_exactly_that_plan(tc: TestCase) {
    let plans = draw_plans(&tc);
    for &p in &plans {
        let text = select::shortest_unique(p, &plans).text(p);
        assert_eq!(
            picks(&text, &plans),
            vec![p],
            "{text:?} should pick only {}",
            fmt(p)
        );
    }
}

// The footer's "Earlier:" selector picks exactly the plans not yet shown.
#[hegel::test(test_cases = 256, database = None)]
fn earlier_page_is_exactly_the_rest(tc: TestCase) {
    let plans = draw_plans(&tc);
    let from = tc.draw(gs::optional(
        gs::integers::<i64>().min_value(0).max_value(3000),
    ));
    let range = match from {
        Some(o) => format!("{}..", fmt(day(o))),
        None => "..".to_string(),
    };
    let all = picks(&range, &plans);
    if all.len() < 2 {
        return;
    }
    let shown = tc.draw(
        gs::integers::<usize>()
            .min_value(1)
            .max_value(all.len() - 1),
    );
    let Selector::Set(set) = select::parse(&range).unwrap() else {
        unreachable!()
    };
    let next = select::earlier(&set, TODAY, &plans, all[shown - 1]).expect("ranges continue");
    assert_eq!(
        picks(&next, &plans),
        all[shown..].to_vec(),
        "{range} then {next}"
    );
}

// Ranges are inclusive filters over the plans that exist; open ends are open.
#[hegel::test(test_cases = 256, database = None)]
fn ranges_filter_inclusively(tc: TestCase) {
    let plans = draw_plans(&tc);
    let a = day(tc.draw(gs::integers::<i64>().min_value(0).max_value(3000)));
    let b = day(tc.draw(gs::integers::<i64>().min_value(0).max_value(3000)));
    let within = |lo: Date, hi: Date| {
        newest_first(
            plans
                .iter()
                .copied()
                .filter(|d| lo <= *d && *d <= hi)
                .collect(),
        )
    };
    assert_eq!(
        picks(&format!("{}..{}", fmt(a), fmt(b)), &plans),
        within(a, b)
    );
    assert_eq!(
        picks(&format!("{}..", fmt(a)), &plans),
        within(a, Date::MAX)
    );
    assert_eq!(
        picks(&format!("..{}", fmt(b)), &plans),
        within(Date::MIN, b)
    );
    assert_eq!(picks("..", &plans), newest_first(plans.clone()));
}

// Months and years are the plans in them, and work as range bounds.
#[hegel::test(test_cases = 256, database = None)]
fn months_and_years_cover_their_days(tc: TestCase) {
    let plans = draw_plans(&tc);
    let p = plans[tc.draw(
        gs::integers::<usize>()
            .min_value(0)
            .max_value(plans.len() - 1),
    )];
    let month = p.strftime("%Y-%m").to_string();
    let year = p.year().to_string();
    let in_month: Vec<Date> = newest_first(
        plans
            .iter()
            .copied()
            .filter(|d| d.year() == p.year() && d.month() == p.month())
            .collect(),
    );
    assert_eq!(picks(&month, &plans), in_month);
    assert_eq!(picks(&format!("{month}..{month}"), &plans), in_month);
    let in_year: Vec<Date> = newest_first(
        plans
            .iter()
            .copied()
            .filter(|d| d.year() == p.year())
            .collect(),
    );
    assert_eq!(picks(&year, &plans), in_year);
}

// Relative days count back from today, in every spelling.
#[hegel::test(test_cases = 256, database = None)]
fn relative_days_count_back_from_today(tc: TestCase) {
    let n = tc.draw(gs::integers::<u32>().max_value(100_000));
    let expected = vec![TODAY.checked_sub((n as i64).days()).unwrap()];
    assert_eq!(picks(&format!("@~{n}"), &[]), expected);
    assert_eq!(picks(&format!("{n} days ago"), &[]), expected);
}

// A full date is that day, whether or not its plan exists.
#[hegel::test(test_cases = 256, database = None)]
fn full_dates_round_trip(tc: TestCase) {
    let d = day(tc.draw(gs::integers::<i64>().min_value(-700_000).max_value(700_000)));
    assert_eq!(picks(&fmt(d), &[]), vec![d]);
}

// Nothing a user can type makes parsing or resolving panic.
#[hegel::test(test_cases = 512, database = None)]
fn arbitrary_input_never_panics(tc: TestCase) {
    let s = tc.draw(gs::text().max_size(24));
    let plans = draw_plans(&tc);
    if let Ok(selector) = select::parse(&s) {
        let _ = select::resolve(&selector, TODAY, &plans);
    }
}
