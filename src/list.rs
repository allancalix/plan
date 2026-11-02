//! The list view: what `plan <selector>` shows when it matches several plans.
//!
//! ```text
//! 2026-11-01  Sun  @     4 entries
//! 2026-10-28  Wed  @~4   7 entries
//! 30 of 412 plans. Earlier: plan ..2026-03-13
//! ```
//!
//! Each date's shortest unique suffix (see [`select::shortest_unique`]) is
//! highlighted, so typing it picks that plan. Piped output is the same rows,
//! uncoloured and never cut short.

use crate::select;
use jiff::civil::Date;
use std::fmt::Write as _;

/// One plan in the list.
pub struct Row {
    pub date: Date,
    /// Entries in its inbox.
    pub entries: usize,
}

const DIM: &str = "\x1b[2m";
const ID: &str = "\x1b[1;35m";
const RESET: &str = "\x1b[0m";

/// The `@`/`@~N` selector for `date`, or nothing for a future date.
fn relative(date: Date, today: Date) -> String {
    match date.until(today).map(|s| s.get_days()) {
        Ok(0) => "@".to_string(),
        Ok(n) if n > 0 => format!("@~{n}"),
        _ => String::new(),
    }
}

/// Render `shown` (newest first), out of `total` matching plans. If some were
/// left out, end with a footer; `earlier` gives the selector for them.
pub fn render(
    shown: &[Row],
    total: usize,
    today: Date,
    plans: &[Date],
    color: bool,
    earlier: impl Fn(Date) -> Option<String>,
) -> String {
    let rels: Vec<String> = shown.iter().map(|r| relative(r.date, today)).collect();
    let width = rels.iter().map(String::len).max().unwrap_or(0);
    let mut out = String::new();
    for (row, rel) in shown.iter().zip(&rels) {
        let full = row.date.strftime("%Y-%m-%d").to_string();
        let id = select::shortest_unique(row.date, plans).text(row.date);
        let date = if color {
            let prefix = &full[..full.len() - id.len()];
            format!("{DIM}{prefix}{RESET}{ID}{id}{RESET}")
        } else {
            full
        };
        let noun = if row.entries == 1 { "entry" } else { "entries" };
        let _ = writeln!(
            out,
            "{date}  {}  {rel:<width$}  {} {noun}",
            row.date.strftime("%a"),
            row.entries
        );
    }
    if shown.len() < total {
        let mut footer = format!("{} of {total} plans.", shown.len());
        if let Some(next) = shown.last().and_then(|r| earlier(r.date)) {
            let _ = write!(footer, " Earlier: plan {next}");
        }
        if color {
            footer = format!("{DIM}{footer}{RESET}");
        }
        out.push_str(&footer);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::civil::date;

    const TODAY: Date = date(2026, 2, 19);

    fn rows(dates: &[Date]) -> Vec<Row> {
        dates
            .iter()
            .map(|&d| Row {
                date: d,
                entries: d.day() as usize % 3,
            })
            .collect()
    }

    #[test]
    fn plain_rows_align_and_pluralize() {
        let plans = [date(2026, 2, 19), date(2026, 2, 8), date(2025, 11, 1)];
        let out = render(&rows(&plans), 3, TODAY, &plans, false, |_| None);
        assert_eq!(
            out,
            "2026-02-19  Thu  @      1 entry\n\
             2026-02-08  Sun  @~11   2 entries\n\
             2025-11-01  Sat  @~110  1 entry\n"
        );
    }

    #[test]
    fn color_highlights_the_shortest_unique_suffix() {
        let plans = [date(2026, 2, 1), date(2025, 11, 1), date(2026, 2, 8)];
        let out = render(&rows(&plans[2..]), 1, TODAY, &plans, true, |_| None);
        assert!(
            out.starts_with(&format!("{DIM}2026-02-{RESET}{ID}08{RESET}  Sun")),
            "{out:?}"
        );
        let out = render(&rows(&plans[..1]), 1, TODAY, &plans, true, |_| None);
        assert!(
            out.starts_with(&format!("{DIM}2026-{RESET}{ID}02-01{RESET}")),
            "{out:?}"
        );
    }

    #[test]
    fn footer_says_how_to_see_the_rest() {
        let plans: Vec<Date> = (1..=5).rev().map(|d| date(2026, 2, d)).collect();
        let out = render(&rows(&plans[..2]), 5, TODAY, &plans, false, |d| {
            Some(format!("..{}", d.yesterday().unwrap()))
        });
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), 3);
        assert!(lines[1].starts_with("2026-02-04"));
        assert_eq!(lines[2], "2 of 5 plans. Earlier: plan ..2026-02-03");
        let out = render(&rows(&plans), 5, TODAY, &plans, false, |_| None);
        assert_eq!(out.lines().count(), 5, "no footer when everything fits");
    }
}
