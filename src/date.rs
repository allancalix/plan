use chrono::{Duration, Local, NaiveDate};
use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

/// Get the date for N days ago. If N = 0, today. Takes injectable mock time into account.
pub fn get_date_opt(days_ago: u32) -> Option<NaiveDate> {
    #[cfg(not(feature = "test-clock"))]
    let today = Local::now().naive_local().date();

    #[cfg(feature = "test-clock")]
    let today = {
        if let Ok(mock_time) = std::env::var("PLAN_MOCK_TIME")
            && let Ok(parsed) = NaiveDate::parse_from_str(&mock_time, "%Y-%m-%d")
        {
            parsed
        } else {
            Local::now().naive_local().date()
        }
    };

    today.checked_sub_signed(Duration::days(days_ago as i64))
}

pub fn get_date(days_ago: u32) -> anyhow::Result<NaiveDate> {
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
                "Invalid date format. Use @, @~N, today, yesterday, or 'N days ago'."
            ))
        }
    } else {
        Ok(0)
    }
}

/// Format the date as a filename: YYYY-MM-DD.plan
pub fn format_filename(date: NaiveDate) -> String {
    format!("{}.plan", date.format("%Y-%m-%d"))
}

/// Get the absolute path to a plan file
pub fn get_plan_path(dir: &Path, date: NaiveDate) -> PathBuf {
    dir.join(format_filename(date))
}

/// Generate the initial content for a new plan file
pub fn generate_template(date: NaiveDate) -> String {
    let formatted_date = date.format("%Y, %b %d - %A").to_string();
    let inbox_line = crate::file::make_inbox_line(formatted_date.len());
    let close_line = "~".repeat(formatted_date.len());
    format!(
        "{formatted_date}
{inbox_line}
{close_line}

---
"
    )
}

/// Retrieve the template or read existing content
pub fn ensure_file_exists(path: &Path, date: NaiveDate, is_past: bool) -> io::Result<()> {
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

    // Atomic write
    let tmp_path = path.with_extension(format!("tmp-{}", process::id()));
    let mut tmp_guard = crate::file::TempFileGuard::new(tmp_path.clone());
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(template.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    tmp_guard.persist();

    Ok(())
}
