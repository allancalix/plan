//! What a plan file is allowed to look like, and parsers for every way the
//! harness observes one (`show`, `search`, `ls`, and the raw file on disk).
//!
//! The harness only ever writes inbox entries, so any well-formed plan file is
//! exactly the template with zero or more entries spliced in before the inbox's
//! closing tilde line:
//!
//! ```text
//! 2026, Feb 19 - Thursday      <- header
//! ~~~~~~~~~inbox~~~~~~~~~      <- inbox open
//! 09:41 jx:c3:1               <- entries, in time order
//! 09:42 jx:c0:7
//! ~~~~~~~~~~~~~~~~~~~~~~~      <- inbox close
//!                              <- blank
//! ---
//! Notes for 2026-02-19, line 1: ...  <- ballast prose (seeded files only)
//! ```
//!
//! Two of the dates are seeded with ballast prose before the run. It makes those
//! files bigger than any pipe buffer, so a reader that stops reading really does
//! block a printing `plan` mid-write, and `plan` must never change a byte of it.

use jiff::ToSpan;
use jiff::civil::Date;
use std::collections::BTreeMap;

pub const KEY_FORMAT: &str = "%Y-%m-%d";
/// Every value the harness writes contains this, and `search` queries for it.
pub const ENTRY_PREFIX: &str = "jx:";
/// Lines in an empty template; `ls` line counts are this plus the entry count.
pub const TEMPLATE_LINES: usize = 5;
/// 1-based line number of the first inbox entry.
const FIRST_ENTRY_LINE: usize = 3;
/// Ballast prose lines in a seeded file (about 140KB).
const BALLAST_LINES: usize = 1200;

pub fn key(date: Date) -> String {
    date.strftime(KEY_FORMAT).to_string()
}

/// The date clients consider "today" when no clock fault is active.
pub fn base_date() -> Date {
    jiff::civil::date(2026, 2, 19)
}

pub fn date(key: &str) -> Option<Date> {
    Date::strptime(KEY_FORMAT, key).ok()
}

/// The `HH:MM` an entry starts with, if any. Zero-padded, so it sorts as text.
pub fn entry_time(line: &str) -> Option<&str> {
    let stamp = line.get(..5)?;
    let b = stamp.as_bytes();
    let shaped = b[..2].iter().all(u8::is_ascii_digit)
        && b[2] == b':'
        && b[3..].iter().all(u8::is_ascii_digit)
        && line.as_bytes().get(5).is_none_or(|&c| c == b' ');
    shaped.then_some(stamp)
}

/// Plan files that exist with ballast before the run starts: today and
/// yesterday. Other dates the clock fault reaches start out missing, so
/// creation and "no plan file" paths still get exercised.
pub fn seeded_keys() -> Vec<String> {
    let base = base_date();
    vec![key(base - 1.day()), key(base)]
}

fn is_seeded(key: &str) -> bool {
    seeded_keys().iter().any(|k| k == key)
}

fn ballast(key: &str) -> Vec<String> {
    if !is_seeded(key) {
        return Vec::new();
    }
    (1..=BALLAST_LINES)
        .map(|i| {
            format!(
                "Notes for {key}, line {i}: prose below the inbox that plan must never touch, \
                 here to make the file larger than a pipe buffer."
            )
        })
        .collect()
}

/// A well-formed plan file holding `entries`, ballast included if seeded.
pub fn render(key: &str, entries: &[&str]) -> String {
    let template = template(key).unwrap();
    let lines: Vec<&str> = template.lines().collect();
    let ballast = ballast(key);
    let mut out: Vec<&str> = lines[..2].to_vec();
    out.extend_from_slice(entries);
    out.extend_from_slice(&lines[2..]);
    out.extend(ballast.iter().map(String::as_str));
    out.join("\n") + "\n"
}

/// A temp file from an atomic write of a plan file: `YYYY-MM-DD.<pid>.tmp`.
pub fn is_temp_file(name: &str) -> bool {
    let Some((key, rest)) = name.split_once('.') else {
        return false;
    };
    let pid = rest.strip_suffix(".tmp").unwrap_or("");
    date(key).is_some() && !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())
}

fn template(key: &str) -> Result<String, String> {
    let date = date(key).ok_or_else(|| format!("{key:?} is not a plan date"))?;
    Ok(plan::date::generate_template(date))
}

/// Parse a complete plan file, returning its inbox entries in order.
pub fn parse_plan(key: &str, content: &str) -> Result<Vec<String>, String> {
    let template = template(key)?;
    let expected: Vec<&str> = template.lines().collect();
    debug_assert_eq!(expected.len(), TEMPLATE_LINES);

    if content.is_empty() {
        return Err("file is empty".into());
    }
    if !content.ends_with('\n') {
        return Err(format!("missing trailing newline: {}", snippet(content)));
    }
    let lines: Vec<&str> = content.lines().collect();
    let ballast = ballast(key);
    let min = TEMPLATE_LINES + ballast.len();
    if lines.len() < min {
        return Err(format!(
            "only {} lines, this plan file has at least {min}: {}",
            lines.len(),
            snippet(content)
        ));
    }
    let (lines, prose) = lines.split_at(lines.len() - ballast.len());
    if let Some(i) = (0..prose.len()).find(|&i| prose[i] != ballast[i]) {
        return Err(format!(
            "ballast prose line {} changed to {:?}",
            i + 1,
            prose[i]
        ));
    }
    let (head, rest) = lines.split_at(2);
    let (entries, tail) = rest.split_at(rest.len() - 3);
    if head != &expected[..2] {
        return Err(format!("bad header/inbox open: {head:?}"));
    }
    if tail != &expected[2..] {
        return Err(format!("bad inbox close/footer: {tail:?}"));
    }
    for (i, entry) in entries.iter().enumerate() {
        let t = entry.trim();
        if t.is_empty() || t.chars().all(|c| c == '~') {
            return Err(format!("inbox line {} is {entry:?}", i + FIRST_ENTRY_LINE));
        }
    }
    Ok(entries.iter().map(|s| s.to_string()).collect())
}

/// Parse `plan search jx:` output: `YYYY-MM-DD.plan:LINE: CONTENT`.
///
/// Every inbox entry contains the query, so the matches in each file must be
/// exactly the inbox, on consecutive lines starting right after the inbox open.
pub fn parse_search(out: &str) -> Result<BTreeMap<String, Vec<String>>, String> {
    let mut files: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for line in out.lines() {
        let bad = || format!("unparseable search line {line:?}");
        let (file, rest) = line.split_once(':').ok_or_else(bad)?;
        let (lineno, content) = rest.split_once(": ").ok_or_else(bad)?;
        let key = file.strip_suffix(".plan").ok_or_else(bad)?;
        let lineno: usize = lineno.parse().map_err(|_| bad())?;
        let entries = files.entry(key.to_string()).or_default();
        let expected = FIRST_ENTRY_LINE + entries.len();
        if lineno != expected {
            return Err(format!(
                "{key}: match {content:?} on line {lineno}, expected line {expected}"
            ));
        }
        entries.push(content.to_string());
    }
    Ok(files)
}

/// Parse `plan ..` output: `YYYY-MM-DD  Thu  @~N  K entries`, one line per plan,
/// newest first. The relative column is blank for dates after today.
pub fn parse_list(out: &str) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();
    let mut previous: Option<&str> = None;
    for line in out.lines() {
        let bad = || format!("unparseable list line {line:?}");
        let parts: Vec<&str> = line.split_whitespace().collect();
        let (key, n, noun) = match parts[..] {
            [key, _dow, rel, n, noun] if rel.starts_with('@') => (key, n, noun),
            [key, _dow, n, noun] => (key, n, noun),
            _ => return Err(bad()),
        };
        let n: usize = n.parse().map_err(|_| bad())?;
        if noun != if n == 1 { "entry" } else { "entries" } || date(key).is_none() {
            return Err(bad());
        }
        if previous.is_some_and(|p| p <= key) {
            return Err(format!("list is not newest first: {previous:?} then {key}"));
        }
        previous = Some(key);
        counts.insert(key.to_string(), n);
    }
    Ok(counts)
}

fn snippet(s: &str) -> String {
    const MAX: usize = 200;
    if s.len() <= MAX {
        format!("{s:?}")
    } else {
        let mut end = MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        format!("{:?}...", &s[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Seeded with ballast.
    const K: &str = "2026-02-19";
    /// Reachable by the clock fault, but not seeded.
    const BARE: &str = "2026-02-20";

    #[test]
    fn template_round_trips() {
        let empty = render(BARE, &[]);
        assert_eq!(empty.lines().count(), TEMPLATE_LINES);
        assert_eq!(parse_plan(BARE, &empty), Ok(vec![]));
        let full = render(BARE, &["* jx:c0:1", "jx:c1:1"]);
        assert_eq!(
            parse_plan(BARE, &full),
            Ok(vec!["* jx:c0:1".into(), "jx:c1:1".into()])
        );
    }

    #[test]
    fn seeded_files_are_bigger_than_a_pipe_buffer() {
        let full = render(K, &["* jx:c0:1"]);
        assert!(full.len() > 128 * 1024, "only {} bytes", full.len());
        // Ballast must be invisible to `search jx:`.
        assert_eq!(full.to_lowercase().matches(ENTRY_PREFIX).count(), 1);
        assert_eq!(parse_plan(K, &full), Ok(vec!["* jx:c0:1".into()]));
        let bare = render(BARE, &["* jx:c0:1"]);
        assert!(parse_plan(K, &bare).is_err(), "ballast missing");
        let tampered = full.replace("line 600:", "line 600 (edited):");
        assert!(parse_plan(K, &tampered).unwrap_err().contains("ballast"));
    }

    #[test]
    fn rejects_torn_and_mangled_files() {
        for key in [K, BARE] {
            let full = render(key, &["* jx:c0:1"]);
            assert!(parse_plan(key, &full[..full.len() / 2]).is_err());
            assert!(parse_plan(key, "").is_err());
            let doubled = full.replace("* jx:c0:1\n", "* jx:c0:1\n~~~~\n");
            assert!(parse_plan(key, &doubled).is_err());
        }
        assert!(
            parse_plan("2026-02-21", &render(BARE, &[])).is_err(),
            "wrong header date"
        );
    }

    #[test]
    fn parses_search_and_list() {
        let out = "2026-02-19.plan:3: * jx:c0:1\n2026-02-19.plan:4: jx:c1:1\n";
        let m = parse_search(out).unwrap();
        assert_eq!(m[K], vec!["* jx:c0:1", "jx:c1:1"]);
        assert!(parse_search("2026-02-19.plan:5: jx:c1:1\n").is_err());

        let m = parse_list("2026-02-20  Fri       1 entry\n2026-02-19  Thu  @     2 entries\n")
            .unwrap();
        assert_eq!((m[BARE], m[K]), (1, 2));
        assert!(
            parse_list("2026-02-19  Thu  @  2 entry\n").is_err(),
            "noun must agree"
        );
        assert!(
            parse_list("2026-02-19  Thu  @  1 entry\n2026-02-20  Fri  1 entry\n").is_err(),
            "order"
        );
    }
}
