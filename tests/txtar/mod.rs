// Vendored from simple_txtar 1.1.0 (https://crates.io/crates/simple_txtar)
// License: MIT OR Apache-2.0
//
// A simple implementation of the Go txtar package: a trivial text-based file archive format.
// https://github.com/golang/tools/blob/master/txtar/archive.go
use std::{fmt, iter::IntoIterator, ops::Index, slice::Iter};

const NEWLINE_MARKER: &str = "\n-- ";
const MARKER: &str = "-- ";
const MARKER_END: &str = " --";
const MARKER_LEN: usize = MARKER.len() + MARKER_END.len();

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Builder {
    inner: Archive,
}

impl Builder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn comment(&mut self, comment: impl Into<String>) -> &mut Self {
        self.inner.comment = comment.into();
        self
    }

    pub fn file(&mut self, file: impl Into<File>) -> &mut Self {
        self.inner.files.push(file.into());
        self
    }

    pub fn build(self) -> Archive {
        self.inner
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Archive {
    comment: String,
    files: Vec<File>,
}

impl fmt::Display for Archive {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", fix_trailing_newline(&self.comment))?;
        for file in self.files.iter() {
            write!(f, "{file}")?;
        }
        Ok(())
    }
}

impl Archive {
    pub fn comment(&self) -> &str {
        &self.comment
    }

    pub fn iter(&self) -> Iter<'_, File> {
        self.files.iter()
    }
}

impl Index<usize> for Archive {
    type Output = File;
    fn index(&self, index: usize) -> &Self::Output {
        &self.files[index]
    }
}

impl Index<&str> for Archive {
    type Output = File;
    fn index(&self, index: &str) -> &Self::Output {
        self.files
            .iter()
            .find(|f| f.name == index)
            .expect("unknown file")
    }
}

impl IntoIterator for Archive {
    type Item = File;
    type IntoIter = std::vec::IntoIter<File>;
    fn into_iter(self) -> Self::IntoIter {
        self.files.into_iter()
    }
}

impl From<&str> for Archive {
    fn from(s: &str) -> Self {
        let (comment, mut name_after) = find_file_marker(s);
        let mut a = Archive {
            comment,
            files: Vec::new(),
        };

        let mut content;
        while let Some((name, after)) = name_after {
            (content, name_after) = find_file_marker(after);
            a.files.push(File::new(name, content));
        }

        a
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct File {
    pub name: String,
    pub content: String,
}

impl File {
    pub fn new(name: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            content: content.into(),
        }
    }
}

impl fmt::Display for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "-- {} --", self.name)?;
        write!(f, "{}", fix_trailing_newline(&self.content))
    }
}

impl<T, U> From<(T, U)> for File
where
    T: Into<String>,
    U: Into<String>,
{
    fn from((name, content): (T, U)) -> Self {
        Self {
            name: name.into(),
            content: content.into(),
        }
    }
}

fn fix_trailing_newline(s: &str) -> String {
    let mut s = s.to_string();
    if !(s.is_empty() || s.ends_with('\n')) {
        s.push('\n');
    }
    s
}

fn find_file_marker(s: &str) -> (String, Option<(&str, &str)>) {
    let mut i = 0;

    loop {
        let (before, after) = s.split_at(i);
        let name_after = try_parse_marker(after);
        if name_after.is_some() {
            return (before.to_string(), name_after);
        }

        match after.find(NEWLINE_MARKER) {
            Some(j) => i += j + 1,
            None => return (fix_trailing_newline(s), None),
        };
    }
}

fn try_parse_marker(s: &str) -> Option<(&str, &str)> {
    if !s.starts_with(MARKER) {
        return None;
    }

    let (s, after) = match s.find('\n') {
        Some(i) => {
            let (s, after) = s.split_at(i);
            (s, after.split_at(1).1)
        }
        None => (s, ""),
    };

    if !(s.ends_with(MARKER_END) && s.len() >= MARKER_LEN) {
        return None;
    }

    let (_, s) = s.split_at(MARKER.len());
    let (s, _) = s.split_at(s.len() - MARKER_END.len());

    Some((s.trim(), after))
}
