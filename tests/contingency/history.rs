//! The history: an append-only log of every invocation, completion, and
//! nemesis action, in the order they happened.
//!
//! Each client operation appears twice: once as an `invoke` event
//! recorded *before* the `plan` process is spawned, and once as a completion
//! (`ok`, `fail`, or `info`) recorded *after* the process has been reaped. The
//! window between the two is the only thing the checker assumes about when the
//! operation took effect.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::time::Instant;

pub const NEMESIS: &str = "nemesis";
pub const FINAL: &str = "final";
/// Seeds plan files before the workload starts.
pub const SETUP: &str = "setup";
/// Collects abandoned processes; reports any that never finished.
pub const REAPER: &str = "reaper";

#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "lowercase")]
pub enum Type {
    #[default]
    Invoke,
    /// The operation definitely happened.
    Ok,
    /// The operation definitely did not happen.
    Fail,
    /// The operation may or may not have happened (crashed, killed, timed out).
    Info,
}

impl Type {
    pub fn as_str(self) -> &'static str {
        match self {
            Type::Invoke => "invoke",
            Type::Ok => "ok",
            Type::Fail => "fail",
            Type::Info => "info",
        }
    }
}

#[derive(
    Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
#[serde(rename_all = "kebab-case")]
pub enum F {
    #[default]
    Append,
    Read,
    Create,
    // Nemesis actions.
    Kill,
    Pause,
    Resume,
    /// A fault window opened (value: the fault name).
    Start,
    /// A fault window closed (value: the fault name).
    Stop,
    ClockSkew,
    ClockReset,
    RogueWrite,
}

impl F {
    pub fn as_str(self) -> &'static str {
        match self {
            F::Append => "append",
            F::Read => "read",
            F::Create => "create",
            F::Kill => "kill",
            F::Pause => "pause",
            F::Resume => "resume",
            F::Start => "start",
            F::Stop => "stop",
            F::ClockSkew => "clock-skew",
            F::ClockReset => "clock-reset",
            F::RogueWrite => "rogue-write",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum Value {
    #[default]
    None,
    /// An append: the exact line expected to land in the inbox.
    Entry(String),
    /// A read (`show`, `search`): plan file key -> inbox lines, in file order.
    Entries(BTreeMap<String, Vec<String>>),
    /// A read (`ls`): plan file key -> number of inbox entries.
    Counts(BTreeMap<String, usize>),
    /// A read whose output could not be parsed as a well-formed plan file.
    Malformed(String),
    /// Free-form description (nemesis events).
    Text(String),
}

impl Value {
    pub fn is_none(&self) -> bool {
        matches!(self, Value::None)
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct Event {
    pub index: usize,
    /// Nanoseconds since the start of the run.
    pub time: u64,
    pub process: String,
    #[serde(rename = "type")]
    pub typ: Type,
    pub f: F,
    /// Target plan file (`YYYY-MM-DD`). `None` on a read means "every file".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Value::is_none")]
    pub value: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub argv: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mock_date: Option<String>,
    /// Nemesis events: history index of the invocation they targeted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<usize>,
    /// Faults that touched this operation (stdio sabotage on invoke, signals
    /// delivered on completion).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub faults: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The "unexpected files" warning plan printed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

pub struct Recorder {
    start: Instant,
    events: Mutex<Vec<Event>>,
}

impl Recorder {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
            events: Mutex::new(Vec::new()),
        }
    }

    /// Append an event, stamping its index and time. The timestamp is taken
    /// under the lock so that index order and time order always agree.
    pub fn record(&self, mut event: Event) -> usize {
        let mut events = self.events.lock().unwrap_or_else(|e| e.into_inner());
        let index = events.len();
        event.index = index;
        event.time = self.start.elapsed().as_nanos() as u64;
        events.push(event);
        index
    }

    pub fn events(&self) -> Vec<Event> {
        self.events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

pub fn write_jsonl(path: &Path, events: &[Event]) -> io::Result<()> {
    let mut out = BufWriter::new(File::create(path)?);
    for event in events {
        serde_json::to_writer(&mut out, event)?;
        out.write_all(b"\n")?;
    }
    out.flush()
}

pub fn read_jsonl(path: &Path) -> io::Result<Vec<Event>> {
    BufReader::new(File::open(path)?)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?))
        .collect()
}
