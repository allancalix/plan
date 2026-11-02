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
//! * jx:c3:1                    <- entries, in commit order
//! jx:c0:7
//! ~~~~~~~~~~~~~~~~~~~~~~~      <- inbox close
//!                              <- blank
//! ---
//! ```

use chrono::NaiveDate;
use std::collections::BTreeMap;

pub const KEY_FORMAT: &str = "%Y-%m-%d";
/// Every value the harness writes contains this, and `search` queries for it.
pub const ENTRY_PREFIX: &str = "jx:";
/// Lines in an empty template; `ls` line counts are this plus the entry count.
pub const TEMPLATE_LINES: usize = 5;
/// 1-based line number of the first inbox entry.
const FIRST_ENTRY_LINE: usize = 3;

pub fn key(date: NaiveDate) -> String {
    date.format(KEY_FORMAT).to_string()
}

pub fn date(key: &str) -> Option<NaiveDate> {
    NaiveDate::parse_from_str(key, KEY_FORMAT).ok()
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
    if lines.len() < TEMPLATE_LINES {
        return Err(format!(
            "only {} lines, a plan file has at least {TEMPLATE_LINES}: {}",
            lines.len(),
            snippet(content)
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

/// Parse `plan ls` output: `YYYY-MM-DD  Thu  NN lines`.
pub fn parse_ls(out: &str) -> Result<BTreeMap<String, usize>, String> {
    let mut counts = BTreeMap::new();
    for line in out.lines() {
        let bad = || format!("unparseable ls line {line:?}");
        let parts: Vec<&str> = line.split_whitespace().collect();
        let [key, _dow, n, "lines"] = parts[..] else {
            return Err(bad());
        };
        let n: usize = n.parse().map_err(|_| bad())?;
        let entries = n
            .checked_sub(TEMPLATE_LINES)
            .ok_or_else(|| format!("{key}: {n} lines is shorter than the template"))?;
        counts.insert(key.to_string(), entries);
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

/// Build a well-formed plan file. Used by checker tests.
#[cfg(test)]
pub fn render_plan(key: &str, entries: &[&str]) -> String {
    let template = template(key).unwrap();
    let lines: Vec<&str> = template.lines().collect();
    let mut out: Vec<&str> = lines[..2].to_vec();
    out.extend_from_slice(entries);
    out.extend_from_slice(&lines[2..]);
    out.join("\n") + "\n"
}

#[cfg(test)]
mod tests {
    use super::*;

    const K: &str = "2026-02-19";

    #[test]
    fn template_round_trips() {
        let empty = render_plan(K, &[]);
        assert_eq!(empty.lines().count(), TEMPLATE_LINES);
        assert_eq!(parse_plan(K, &empty), Ok(vec![]));
        let full = render_plan(K, &["* jx:c0:1", "jx:c1:1"]);
        assert_eq!(
            parse_plan(K, &full),
            Ok(vec!["* jx:c0:1".into(), "jx:c1:1".into()])
        );
    }

    #[test]
    fn rejects_torn_and_mangled_files() {
        let full = render_plan(K, &["* jx:c0:1"]);
        assert!(parse_plan(K, &full[..full.len() / 2]).is_err());
        assert!(parse_plan(K, "").is_err());
        assert!(
            parse_plan("2026-02-20", &full).is_err(),
            "wrong header date"
        );
        let doubled = full.replace("* jx:c0:1\n", "* jx:c0:1\n~~~~\n");
        assert!(parse_plan(K, &doubled).is_err());
    }

    #[test]
    fn parses_search_and_ls() {
        let out = "2026-02-19.plan:3: * jx:c0:1\n2026-02-19.plan:4: jx:c1:1\n";
        let m = parse_search(out).unwrap();
        assert_eq!(m[K], vec!["* jx:c0:1", "jx:c1:1"]);
        assert!(parse_search("2026-02-19.plan:5: jx:c1:1\n").is_err());

        let m = parse_ls("2026-02-19  Thu   7 lines\n").unwrap();
        assert_eq!(m[K], 2);
        assert!(parse_ls("2026-02-19  Thu   3 lines\n").is_err());
    }
}
