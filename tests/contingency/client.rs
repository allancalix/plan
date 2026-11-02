//! Clients: each one is a thread that behaves like a user (or a script) calling
//! `plan` over and over, one process at a time.
//!
//! A client generates a random operation, records the invocation, spawns `plan`,
//! supervises it (delivering nemesis signals, enforcing a timeout), reaps it,
//! and records what happened. Clients never judge correctness; they only report
//! what they saw. That's the checker's job.

use crate::history::{Event, F, Recorder, Type, Value};
use crate::model::{self, ENTRY_PREFIX};
use crate::nemesis::Control;
use crate::sandbox::Sandbox;
use chrono::NaiveDate;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read, Write};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_micros(200);
const WARNING_PREFIX: &str = "plan: warning: unexpected files";

pub struct Ctx<'a> {
    pub plan_bin: &'a Path,
    pub sandbox: &'a Sandbox,
    pub rec: &'a Recorder,
    pub ctl: &'a Control,
    pub base_date: NaiveDate,
    pub timeout: Duration,
}

#[derive(Clone, Debug)]
pub enum Kind {
    /// `plan log|jot <text|-> [date]`; `line` is what should land in the inbox.
    Append {
        stdin: bool,
        text: String,
        line: String,
    },
    /// `plan show [date]`
    Show,
    /// `plan search jx:`
    Search,
    /// `plan ls`
    Ls,
    /// `plan --path [date]`: creates the file from the template if missing.
    Create,
}

pub struct Op {
    kind: Kind,
    /// Target file; `None` for whole-directory reads.
    key: Option<String>,
    mock_date: String,
    argv: Vec<String>,
}

impl Op {
    fn new(kind: Kind, verb: &str, mock: NaiveDate, days_ago: u32, date_arg: Option<&str>) -> Self {
        let mut argv: Vec<String> = match &kind {
            Kind::Append { stdin, text, .. } => {
                vec![verb.into(), if *stdin { "-".into() } else { text.clone() }]
            }
            Kind::Show => vec!["show".into()],
            Kind::Search => vec!["search".into(), ENTRY_PREFIX.into()],
            Kind::Ls => vec!["ls".into()],
            Kind::Create => vec!["--path".into()],
        };
        let key = match kind {
            Kind::Search | Kind::Ls => None,
            _ => {
                argv.extend(date_arg.map(String::from));
                Some(model::key(mock - chrono::Duration::days(days_ago.into())))
            }
        };
        Self {
            kind,
            key,
            mock_date: model::key(mock),
            argv,
        }
    }

    fn f(&self) -> F {
        match self.kind {
            Kind::Append { .. } => F::Append,
            Kind::Create => F::Create,
            _ => F::Read,
        }
    }
}

/// Stdio sabotage applied at spawn time while the `pipe` fault is active.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StdioFault {
    StdoutClosed,
    StderrClosed,
    StdinClosed,
    StdinPartial,
}

impl StdioFault {
    fn name(self) -> &'static str {
        match self {
            StdioFault::StdoutClosed => "stdout-closed",
            StdioFault::StderrClosed => "stderr-closed",
            StdioFault::StdinClosed => "stdin-closed",
            StdioFault::StdinPartial => "stdin-partial",
        }
    }
}

pub struct Client<'a> {
    id: usize,
    name: String,
    rng: StdRng,
    ctx: &'a Ctx<'a>,
    seq: u64,
}

impl<'a> Client<'a> {
    pub fn new(id: usize, name: String, seed: u64, ctx: &'a Ctx<'a>) -> Self {
        Self {
            id,
            name,
            rng: StdRng::seed_from_u64(seed),
            ctx,
            seq: 0,
        }
    }

    pub fn run_until_stopped(&mut self) {
        while !self.ctx.ctl.stopped() {
            let op = self.generate();
            self.invoke(op);
        }
    }

    /// Quiescent reads after the nemesis has healed: every file via `show`,
    /// then `search` and `ls` across the directory.
    pub fn final_reads(&mut self, keys: &BTreeSet<String>) {
        for key in keys {
            if let Some(date) = model::date(key) {
                self.invoke(Op::new(Kind::Show, "", date, 0, None));
            }
        }
        let base = self.ctx.base_date;
        self.invoke(Op::new(Kind::Search, "", base, 0, None));
        self.invoke(Op::new(Kind::Ls, "", base, 0, None));
    }

    fn generate(&mut self) -> Op {
        let rng = &mut self.rng;
        let mock = self.ctx.base_date + chrono::Duration::days(self.ctx.ctl.skew(self.id));
        let days_ago = if rng.random_bool(0.1) { 1 } else { 0 };
        let date_arg: Option<&str> = if days_ago == 0 {
            *[None, None, Some("@"), Some("today")].choose(rng).unwrap()
        } else {
            *[Some("@~1"), Some("yesterday"), Some("1 day ago")]
                .choose(rng)
                .unwrap()
        };
        let roll = rng.random_range(0..100);
        let (kind, verb) = match roll {
            0..55 => {
                self.seq += 1;
                let text = format!("{ENTRY_PREFIX}{}:{}", self.name, self.seq);
                let stdin = rng.random_bool(0.2);
                if rng.random_bool(0.6) {
                    let line = format!("* {text}");
                    (Kind::Append { stdin, text, line }, "log")
                } else {
                    let line = text.clone();
                    (Kind::Append { stdin, text, line }, "jot")
                }
            }
            55..75 => (Kind::Show, ""),
            75..83 => (Kind::Search, ""),
            83..91 => (Kind::Ls, ""),
            _ => (Kind::Create, ""),
        };
        Op::new(kind, verb, mock, days_ago, date_arg)
    }

    fn pick_stdio_fault(&mut self, op: &Op) -> Option<StdioFault> {
        if !self.ctx.ctl.pipe_active() || !self.rng.random_bool(0.5) {
            return None;
        }
        let mut options = vec![StdioFault::StdoutClosed, StdioFault::StderrClosed];
        if matches!(op.kind, Kind::Append { stdin: true, .. }) {
            options.extend([StdioFault::StdinClosed, StdioFault::StdinPartial]);
        }
        options.choose(&mut self.rng).copied()
    }

    fn invoke(&mut self, op: Op) {
        let ctx = self.ctx;
        let stdio_fault = self.pick_stdio_fault(&op);
        let entry = match &op.kind {
            Kind::Append { line, .. } => Value::Entry(line.clone()),
            _ => Value::None,
        };
        let stdin_text = match &op.kind {
            Kind::Append {
                stdin: true, text, ..
            } => Some(text.clone()),
            _ => None,
        };

        let mut cmd = ctx.sandbox.command(ctx.plan_bin, &op.mock_date);
        cmd.args(&op.argv)
            .stdin(if stdin_text.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let invoke_index = ctx.rec.record(Event {
            process: self.name.clone(),
            typ: Type::Invoke,
            f: op.f(),
            key: op.key.clone(),
            value: entry.clone(),
            argv: op.argv.clone(),
            mock_date: Some(op.mock_date.clone()),
            faults: stdio_fault
                .map(|f| f.name().to_string())
                .into_iter()
                .collect(),
            ..Event::default()
        });
        let started = Instant::now();

        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                // The process never existed, so the op definitely didn't happen.
                self.record_completion(
                    &op,
                    Type::Fail,
                    entry,
                    vec![],
                    None,
                    None,
                    Some(format!("spawn: {e}")),
                    None,
                );
                return;
            }
        };
        let pid = child.id() as libc::pid_t;
        ctx.ctl.begin(self.id, invoke_index);

        if let (Some(mut stdin), Some(text)) = (child.stdin.take(), stdin_text) {
            // Write errors are expected if the child already died.
            let _ = match stdio_fault {
                Some(StdioFault::StdinClosed) => Ok(()),
                Some(StdioFault::StdinPartial) => stdin.write_all(text.as_bytes()),
                _ => stdin.write_all(format!("{text}\n").as_bytes()),
            };
        }
        let stdout = reader(
            child.stdout.take(),
            stdio_fault == Some(StdioFault::StdoutClosed),
        );
        let stderr = reader(
            child.stderr.take(),
            stdio_fault == Some(StdioFault::StderrClosed),
        );

        let mut delivered = Vec::new();
        let mut timed_out = false;
        let status: io::Result<ExitStatus> = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {}
                Err(e) => {
                    // Still unreaped, so the pid is still ours to kill.
                    let _ = child.kill();
                    let _ = child.wait();
                    break Err(e);
                }
            }
            for sig in ctx.ctl.take_signals(self.id) {
                // SAFETY: plain syscall; `pid` is our unreaped child.
                unsafe { libc::kill(pid, sig.raw()) };
                delivered.push(sig.name().to_string());
            }
            if !timed_out && started.elapsed() > ctx.timeout {
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
                timed_out = true;
                delivered.push("timeout".to_string());
            }
            thread::sleep(POLL);
        };
        ctx.ctl.end(self.id);

        let stdout = join(stdout);
        let stderr = join(stderr);
        // plan's "unexpected files" warning is not an error, but it is user-visible
        // noise worth reporting, so it gets its own field.
        let (warnings, stderr): (Vec<&str>, Vec<&str>) =
            stderr.lines().partition(|l| l.starts_with(WARNING_PREFIX));
        let warning = warnings.first().map(|w| w.to_string());
        let stderr = stderr.join("\n");

        let (exit, signal) = match &status {
            Ok(st) => (st.code(), st.signal()),
            Err(_) => (None, None),
        };
        let error = match &status {
            Err(e) => Some(format!("wait: {e}")),
            Ok(_) if !stderr.trim().is_empty() => Some(tail(&stderr)),
            Ok(_) => None,
        };
        let stdout = (stdio_fault != Some(StdioFault::StdoutClosed)).then_some(stdout.as_str());
        let (typ, value) = classify(&op, exit, stdout, &stderr);
        self.record_completion(&op, typ, value, delivered, exit, signal, error, warning);
    }

    #[allow(clippy::too_many_arguments)]
    fn record_completion(
        &self,
        op: &Op,
        typ: Type,
        value: Value,
        faults: Vec<String>,
        exit: Option<i32>,
        signal: Option<i32>,
        error: Option<String>,
        warning: Option<String>,
    ) {
        self.ctx.rec.record(Event {
            process: self.name.clone(),
            typ,
            f: op.f(),
            key: op.key.clone(),
            value,
            faults,
            exit,
            signal,
            error,
            warning,
            ..Event::default()
        });
    }
}

/// Decide what an exit status means.
///
/// For side-effecting ops we only claim `fail` when plan itself reported an
/// error (exit 1 or 2), since those paths all return before the rename that
/// commits a write. Panics, signals, and timeouts are `info`: the write may
/// or may not have landed. Reads that don't succeed are just `fail`; they
/// change nothing, so their outcome doesn't constrain the model. `stdout` is
/// `None` when the harness closed it.
fn classify(op: &Op, exit: Option<i32>, stdout: Option<&str>, stderr: &str) -> (Type, Value) {
    // With stdout sabotaged, a read's result was never observed: even if plan
    // managed to write before we closed the pipe, the bytes are gone.
    let Some(stdout) = stdout.or(match op.kind {
        Kind::Append { .. } | Kind::Create => Some(""),
        _ => None,
    }) else {
        return (Type::Fail, Value::None);
    };
    match (&op.kind, exit) {
        (Kind::Append { line, .. }, Some(0)) => (Type::Ok, Value::Entry(line.clone())),
        (Kind::Append { line, .. }, Some(1 | 2)) => (Type::Fail, Value::Entry(line.clone())),
        (Kind::Append { line, .. }, _) => (Type::Info, Value::Entry(line.clone())),

        (Kind::Create, Some(0)) => (Type::Ok, Value::None),
        (Kind::Create, Some(1 | 2)) => (Type::Fail, Value::None),
        (Kind::Create, _) => (Type::Info, Value::None),

        (Kind::Show, Some(0)) => {
            let key = op.key.clone().unwrap();
            match model::parse_plan(&key, stdout) {
                Ok(entries) => (Type::Ok, Value::Entries(BTreeMap::from([(key, entries)]))),
                Err(reason) => (Type::Ok, Value::Malformed(format!("show {key}: {reason}"))),
            }
        }
        // `show` exits 2 silently when the file doesn't exist: an empty read.
        (Kind::Show, Some(2)) if stderr.trim().is_empty() => {
            let key = op.key.clone().unwrap();
            (Type::Ok, Value::Entries(BTreeMap::from([(key, vec![])])))
        }
        (Kind::Search, Some(0)) => match model::parse_search(stdout) {
            Ok(files) => (Type::Ok, Value::Entries(files)),
            Err(reason) => (Type::Ok, Value::Malformed(format!("search: {reason}"))),
        },
        (Kind::Ls, Some(0)) => match model::parse_ls(stdout) {
            Ok(counts) => (Type::Ok, Value::Counts(counts)),
            Err(reason) => (Type::Ok, Value::Malformed(format!("ls: {reason}"))),
        },
        (Kind::Show | Kind::Search | Kind::Ls, _) => (Type::Fail, Value::None),
    }
}

type Reader = Option<JoinHandle<Vec<u8>>>;

/// Drain a child pipe on its own thread so a chatty child can't block on a
/// full pipe. With `sabotage`, close our end instead so the child hits EPIPE.
fn reader<R: Read + Send + 'static>(pipe: Option<R>, sabotage: bool) -> Reader {
    let mut pipe = pipe?;
    if sabotage {
        return None; // dropping `pipe` closes the read end
    }
    Some(thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    }))
}

fn join(reader: Reader) -> String {
    let bytes = reader.and_then(|h| h.join().ok()).unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn tail(s: &str) -> String {
    const MAX: usize = 500;
    let s = s.trim();
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut start = s.len() - MAX;
    while !s.is_char_boundary(start) {
        start += 1;
    }
    format!("...{}", &s[start..])
}
