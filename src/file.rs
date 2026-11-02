use std::fs::{self, File};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process;

/// Proof that the caller holds the plan directory lock.
pub struct LockGuard {
    _dir: File,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = self._dir.unlock();
    }
}

/// The lock covers the whole plan directory rather than a sidecar file per plan.
///
/// A sidecar can be deleted while it is held (by a user tidying up `*.lock`
/// files, or a sync tool), after which the next writer creates a fresh sidecar,
/// locks it immediately, and races the holder. That loses writes. The plan file
/// itself can't be locked either, because every write replaces it via rename.
/// The directory is the one stable inode, and it can't be removed while it
/// still holds plan files.
fn open_lock_dir(path: &Path) -> io::Result<File> {
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    File::open(dir)
}

/// Acquire an exclusive lock around the target file ensuring serialized IO
pub fn acquire_lock(path: &Path) -> io::Result<LockGuard> {
    let dir = open_lock_dir(path)?;
    dir.lock()?;
    Ok(LockGuard { _dir: dir })
}

/// Acquire a shared lock for read-only operations (allows concurrent readers)
pub fn acquire_shared_lock(path: &Path) -> io::Result<LockGuard> {
    let dir = open_lock_dir(path)?;
    dir.lock_shared()?;
    Ok(LockGuard { _dir: dir })
}

/// Temp file for an atomic write of `path`: `2026-02-19.<pid>.tmp`. The `.tmp`
/// extension keeps it out of the unexpected-files warning, which matters
/// because concurrent `plan` invocations see each other's temp files.
fn temp_path(path: &Path) -> PathBuf {
    path.with_extension(format!("{}.tmp", process::id()))
}

/// Whether `name` is a temp file left by an atomic write of the plan file with
/// stem `stem`: `<stem>.<pid>.tmp`, or `<stem>.tmp-<pid>` from older versions.
fn is_temp_file_for(name: &str, stem: &str) -> bool {
    let Some(rest) = name.strip_prefix(stem).and_then(|r| r.strip_prefix('.')) else {
        return false;
    };
    let pid = rest
        .strip_suffix(".tmp")
        .or_else(|| rest.strip_prefix("tmp-"));
    pid.is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Remove temp files that writers of `path` left behind when they were killed
/// mid-write. Safe because the caller holds the exclusive lock, so no other
/// process can be writing one right now.
fn remove_stale_temp_files(path: &Path, _guard: &LockGuard) {
    let (Some(dir), Some(stem)) = (path.parent(), path.file_stem().and_then(|s| s.to_str())) else {
        return;
    };
    let Ok(entries) = fs::read_dir(if dir.as_os_str().is_empty() {
        Path::new(".")
    } else {
        dir
    }) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| is_temp_file_for(name, stem))
        {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// Replace `path` with `content` atomically: write a temp file, fsync it, and
/// rename it over the target. Readers see the old or the new file, never a mix.
pub fn atomic_write(path: &Path, content: &str, guard: &LockGuard) -> io::Result<()> {
    remove_stale_temp_files(path, guard);
    let tmp_path = temp_path(path);
    let mut tmp_guard = TempFileGuard::new(tmp_path.clone());
    {
        let mut file = File::create(&tmp_path)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, path)?;
    tmp_guard.persist();
    Ok(())
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

/// The inbox fences. They are fixed so they are easy to spot and to parse.
/// Older versions sized them to the header line; those are still recognised
/// (see [`is_inbox_open`]) and get rewritten to these on the next write.
pub const INBOX_OPEN: &str = "~~~~~~~~~inbox~~~~~~~~~";
pub const INBOX_CLOSE: &str = "~~~~~~~~~~~~~~~~~~~~~~~";

/// The `HH:MM` that leads an inbox entry, if it has one.
fn entry_time(line: &str) -> Option<jiff::civil::Time> {
    let b = line.as_bytes();
    let shaped = b.len() >= 5
        && b[..2].iter().all(u8::is_ascii_digit)
        && b[2] == b':'
        && b[3..5].iter().all(u8::is_ascii_digit)
        && b.get(5).is_none_or(|&c| c == b' ');
    if !shaped {
        return None;
    }
    let two = |i: usize| ((b[i] - b'0') * 10 + (b[i + 1] - b'0')) as i8;
    jiff::civil::Time::new(two(0), two(3), 0, 0).ok()
}

fn is_inbox_open(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('~') && t.ends_with('~') && t.contains("inbox") && t.replace('~', "") == "inbox"
}

fn is_tilde_line(line: &str) -> bool {
    let t = line.trim();
    !t.is_empty() && t.chars().all(|c| c == '~')
}

/// The number of entries in the inbox: non-blank lines between its fences.
pub fn inbox_entries(content: &str) -> usize {
    let mut lines = content.lines().skip_while(|l| !is_inbox_open(l));
    if lines.next().is_none() {
        return 0;
    }
    lines
        .take_while(|l| !is_tilde_line(l))
        .filter(|l| !l.trim().is_empty())
        .count()
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
        // Temp files from older versions of plan: `2026-02-19.tmp-<pid>`.
        if ext
            .strip_prefix(".tmp-")
            .is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()))
        {
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
    // A warning must never take plan down, even if stderr is a closed pipe.
    let _ = writeln!(
        io::stderr(),
        "plan: warning: unexpected files in plan directory: {} (suppress with warn_unexpected = false)",
        names
    );
}

/// Insert a line into the inbox of a plan file, then write it atomically.
///
/// The inbox is kept in time order: a line that starts with `HH:MM` goes before
/// the first entry with a later time (after any with the same time), and a line
/// without one goes at the end. Entries without a time never move.
/// If the inbox fences are missing, a new inbox is added at the end of the file.
pub fn insert_into_inbox(path: &Path, new_line: &str, guard: &LockGuard) -> io::Result<()> {
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

    match (inbox_start, inbox_end) {
        (Some(start), Some(end)) => {
            lines[start] = INBOX_OPEN;
            lines[end] = INBOX_CLOSE;
            let at = entry_time(new_line)
                .and_then(|t| {
                    (start + 1..end).find(|&i| entry_time(lines[i]).is_some_and(|x| x > t))
                })
                .unwrap_or(end);
            lines.insert(at, new_line);
        }
        _ => {
            // Edge case: User manually wiped the inbox entirely.
            // Dynamically reconstruct it at the exact end of the file.
            if file_needs_newline {
                lines.push("");
            }
            let mut new_lines = Vec::with_capacity(lines.len() + 3);
            new_lines.extend_from_slice(&lines);
            new_lines.push(INBOX_OPEN);
            new_lines.push(new_line);
            new_lines.push(INBOX_CLOSE);
            let new_content = new_lines.join("\n") + "\n";
            return atomic_write(path, &new_content, guard);
        }
    }

    let new_content = lines.join("\n") + "\n";
    atomic_write(path, &new_content, guard)
}
