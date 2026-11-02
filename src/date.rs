use crate::file::LockGuard;
use jiff::civil::{Date, DateTime};
use jiff::{Span, Zoned};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The current local date and time.
///
/// With the `test-clock` feature, `PLAN_MOCK_TIME` overrides it: either a date
/// (`2026-02-19`, meaning midnight) or a date and time (`2026-02-19T14:32`).
pub fn now() -> DateTime {
    #[cfg(feature = "test-clock")]
    if let Ok(mock) = std::env::var("PLAN_MOCK_TIME") {
        if let Ok(date) = Date::strptime("%Y-%m-%d", &mock) {
            return date.at(0, 0, 0, 0);
        }
        for format in ["%Y-%m-%dT%H:%M", "%Y-%m-%dT%H:%M:%S"] {
            if let Ok(at) = DateTime::strptime(format, &mock) {
                return at;
            }
        }
    }
    Zoned::now().datetime()
}

/// The `HH:MM` timestamp that leads a new inbox entry.
pub fn entry_time() -> String {
    now().strftime("%H:%M").to_string()
}

/// Get the date for N days ago. If N = 0, today. Takes injectable mock time into account.
pub fn get_date_opt(days_ago: u32) -> Option<Date> {
    // `try_days`, not `days`: huge offsets must be an error, not a panic.
    let span = Span::new().try_days(days_ago).ok()?;
    now().date().checked_sub(span).ok()
}

pub fn get_date(days_ago: u32) -> anyhow::Result<Date> {
    get_date_opt(days_ago)
        .ok_or_else(|| anyhow::anyhow!("Date calculation is out of bounds (too far in the past)."))
}

pub fn parse_date_opt(arg: Option<&str>) -> anyhow::Result<u32> {
    if let Some(d) = arg {
        let d_lower = d.trim().to_lowercase();
        if let Some(stripped) = d_lower.strip_prefix("@~") {
            stripped.parse::<u32>().map_err(|_| {
                anyhow::anyhow!(
                    "Invalid relative date '@~{}'. Expected unsigned integer.",
                    stripped
                )
            })
        } else if d_lower == "@" || d_lower == "today" {
            Ok(0)
        } else if d_lower == "yesterday" {
            Ok(1)
        } else if let Some(num_str) = d_lower
            .strip_suffix(" days ago")
            .or_else(|| d_lower.strip_suffix(" day ago"))
        {
            num_str.trim().parse::<u32>().map_err(|_| {
                anyhow::anyhow!(
                    "Invalid date format '{}'. Expected unsigned integer before 'days ago'.",
                    d
                )
            })
        } else {
            Err(anyhow::anyhow!(
                "Invalid date format. Use @, @~N, @-, today, yesterday, or 'N days ago'."
            ))
        }
    } else {
        Ok(0)
    }
}

/// Format the date as a filename: YYYY-MM-DD.plan
pub fn format_filename(date: Date) -> String {
    format!("{}.plan", date.strftime("%Y-%m-%d"))
}

/// Get the absolute path to a plan file
pub fn get_plan_path(dir: &Path, date: Date) -> PathBuf {
    dir.join(format_filename(date))
}

/// Generate the initial content for a new plan file
pub fn generate_template(date: Date) -> String {
    let formatted_date = date.strftime("%Y, %b %d - %A").to_string();
    let inbox_line = crate::file::INBOX_OPEN;
    let close_line = crate::file::INBOX_CLOSE;
    format!(
        "{formatted_date}
{inbox_line}
{close_line}

---
"
    )
}

/// Create the plan file from the template if it doesn't exist yet.
pub fn ensure_file_exists(
    path: &Path,
    date: Date,
    is_past: bool,
    guard: &LockGuard,
) -> io::Result<()> {
    if path.exists() {
        return Ok(());
    }

    if is_past {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("Plan file for past date does not exist: {}", path.display()),
        ));
    }

    let template = generate_template(date);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }

    crate::file::atomic_write(path, &template, guard)
}
