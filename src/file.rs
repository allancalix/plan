use fs4::fs_std::FileExt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::process;

pub struct LockGuard {
    _file: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

/// Acquire an exclusive lock around the target file ensuring serialized IO
pub fn acquire_lock(path: &Path) -> io::Result<LockGuard> {
    let lock_path = path.with_extension("lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock_exclusive()?;
    Ok(LockGuard { _file: lock_file })
}

/// Acquire a shared lock for read-only operations (allows concurrent readers)
pub fn acquire_shared_lock(path: &Path) -> io::Result<LockGuard> {
    let lock_path = path.with_extension("lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)?;
    lock_file.lock_shared()?;
    Ok(LockGuard { _file: lock_file })
}

pub struct TempFileGuard {
    path: std::path::PathBuf,
    persist: bool,
}

impl TempFileGuard {
    pub fn new(path: std::path::PathBuf) -> Self {
        Self {
            path,
            persist: false,
        }
    }
    pub fn persist(&mut self) {
        self.persist = true;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.persist {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Build a centered `~~~~~inbox~~~~~` line of the given total width.
pub fn make_inbox_line(width: usize) -> String {
    let label = "inbox";
    let remaining = width.saturating_sub(label.len());
    let left = remaining / 2;
    let right = remaining - left;
    format!("{}inbox{}", "~".repeat(left), "~".repeat(right))
}

fn is_inbox_open(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('~') && t.ends_with('~') && t.contains("inbox") && t.replace('~', "") == "inbox"
}

fn is_tilde_line(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '~')
}

pub fn is_plan_file(name: &str) -> bool {
    name.ends_with(".plan") && !name.starts_with(".sync-conflict")
}

const IGNORED_NAMES: &[&str] = &[".DS_Store", "Thumbs.db"];
const IGNORED_EXTENSIONS: &[&str] = &[".lock", ".swp", ".tmp"];
const IGNORED_SUFFIXES: &[&str] = &["~"];

fn should_ignore(name: &str, user_patterns: &[String]) -> bool {
    if IGNORED_NAMES.contains(&name) {
        return true;
    }
    if IGNORED_SUFFIXES.iter().any(|s| name.ends_with(s)) {
        return true;
    }
    if let Some(dot) = name.rfind('.') {
        let ext = &name[dot..];
        if IGNORED_EXTENSIONS.contains(&ext) {
            return true;
        }
    }
    for pattern in user_patterns {
        if let Some(suffix) = pattern.strip_prefix('*') {
            if name.ends_with(suffix) {
                return true;
            }
        } else if name == pattern {
            return true;
        }
    }
    false
}

/// Result of scanning a plan directory.
pub struct ScanResult {
    pub plan_entries: Vec<fs::DirEntry>,
    pub unexpected: Vec<String>,
}

/// Scan a plan directory, separating plan files from unexpected files.
/// Only flags regular files; directories are always ignored.
pub fn scan_plan_dir(dir: &Path, user_ignores: &[String]) -> io::Result<ScanResult> {
    let mut plan_entries = Vec::new();
    let mut unexpected = Vec::new();

    for entry in fs::read_dir(dir)?.filter_map(|e| e.ok()) {
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_plan_file(&name) {
            plan_entries.push(entry);
        } else if !should_ignore(&name, user_ignores) {
            unexpected.push(name);
        }
    }

    Ok(ScanResult {
        plan_entries,
        unexpected,
    })
}

pub fn warn_unexpected_files(unexpected: &[String]) {
    if unexpected.is_empty() {
        return;
    }
    let mut sorted: Vec<&str> = unexpected.iter().map(|s| s.as_str()).collect();
    sorted.sort();
    let names = sorted.join(", ");
    eprintln!(
        "plan: warning: unexpected files in plan directory: {} (suppress with warn_unexpected = false)",
        names
    );
}

/// Append a line to the inbox in a plan file.
/// Performs an atomic write to a tempfile, then renames.
pub fn insert_into_inbox(path: &Path, new_line: &str, _guard: &LockGuard) -> io::Result<()> {
    let content = fs::read_to_string(path)?;

    // Find the inbox markers:
    //   open:  ^~+inbox~+$
    //   close: first ^~+$ (all tildes) after open
    let mut lines: Vec<&str> = content.split('\n').collect();
    // remove the last empty split if it exists because of trailing newline
    if lines.last() == Some(&"") {
        lines.pop();
    }

    let mut inbox_start = None;
    let mut inbox_end = None;

    for (i, line) in lines.iter().enumerate() {
        if inbox_start.is_none() && is_inbox_open(line) {
            inbox_start = Some(i);
        } else if inbox_start.is_some() && inbox_end.is_none() && is_tilde_line(line) {
            inbox_end = Some(i);
        }
    }

    let file_needs_newline = !lines.is_empty() && !lines.last().unwrap().is_empty();

    // Determine width from the first line (header) or use a default
    let width = lines.first().map_or(21, |l| l.len().max(21));

    match (inbox_start, inbox_end) {
        (Some(_), Some(end_idx)) => {
            // Standard case: Inbox is present, inject directly before the closing tilde line
            lines.insert(end_idx, new_line);
        }
        _ => {
            // Edge case: User manually wiped the inbox entirely.
            // Dynamically reconstruct it at the exact end of the file.
            if file_needs_newline {
                lines.push("");
            }
            // Use a collected String so we can reference it as &str in the lines vec
            let inbox_open = make_inbox_line(width);
            let inbox_close = "~".repeat(width);
            let mut new_lines = Vec::with_capacity(lines.len() + 3);
            new_lines.extend_from_slice(&lines);
            new_lines.push(&inbox_open);
            new_lines.push(new_line);
            new_lines.push(&inbox_close);
            let new_content = new_lines.join("\n") + "\n";

            let tmp_path = path.with_extension(format!("tmp-{}", process::id()));
            let mut tmp_guard = TempFileGuard::new(tmp_path.clone());
            {
                let mut file = File::create(&tmp_path)?;
                file.write_all(new_content.as_bytes())?;
                file.sync_all()?;
            }
            fs::rename(&tmp_path, path)?;
            tmp_guard.persist();

            return Ok(());
        }
    }

    let new_content = lines.join("\n") + "\n";

    // Atomic write
    let tmp_path = path.with_extension(format!("tmp-{}", process::id()));
    let mut tmp_guard = TempFileGuard::new(tmp_path.clone());
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(new_content.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    tmp_guard.persist();

    Ok(())
}

/// Find the most recent plan file from pre-scanned entries.
pub fn find_latest(entries: &[fs::DirEntry]) -> Option<std::path::PathBuf> {
    entries
        .iter()
        .filter(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let date_str = &name[..name.len() - 5];
            chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d").is_ok()
        })
        .max_by_key(|e| e.file_name())
        .map(|e| e.path())
}
