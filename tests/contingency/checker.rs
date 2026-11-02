//! The checker: decides whether a history is consistent with plan's
//! concurrency model.
//!
//! # The model
//!
//! Each plan file (one per date, the *key*) holds an inbox that behaves as an
//! **append-only list**, and every operation on it must be **linearizable**:
//!
//! * `log`/`jot` appends one entry. An append that exits 0 is durable and
//!   appears exactly once, in the file it targeted. An append that fails
//!   (exit 1/2) never appears. One that crashed (`info`) may or may not.
//! * `show`, `search`, and `ls` observe a **prefix** of the final list:
//!   never a torn, reordered, or partial state, and never an entry that
//!   was later lost.
//! * Real-time order is respected: if A finished before B started, then B
//!   is ordered after A.
//! * `--path` that exits 0 leaves the file in existence, and creating a file
//!   never clobbers entries already in it.
//! * Files are never deleted: once one is known to exist, "no plan file for
//!   that date" is a stale answer.
//!
//! # Why the check is exact and cheap
//!
//! General linearizability checking is NP-complete, but this model gives the
//! game away: appends are serialized, and each one lands at the end of the
//! inbox, so the **final file is the serialization order**. Append `i` (0-based
//! position in the final inbox) linearizes at position `2i+1`, and a read
//! that saw `k` entries must linearize between append `k-1` and append `k`, at
//! position `2k`. A linearization exists iff no operation that finished before
//! another started has a larger position, which an O(n log n) sweep can check.
//!
//! Beyond safety, the checker also reports:
//!
//! * liveness: timeouts, failed final reads, and stalls (one caller waiting on
//!   another caller's trouble, beyond time spent behind deliberately paused
//!   processes);
//! * crashes: any panic, or a signal nobody sent;
//! * hygiene: plan must never print an "unexpected files" warning about its own
//!   files, and must leave nothing in the plan directory but plan files and
//!   temp files it will sweep up.

use crate::history::{Event, F, FINAL, NEMESIS, REAPER, SETUP, Type, Value};
use crate::model;
use crate::sandbox::Snapshot;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Write as _;
use std::time::Duration;

const MAX_EXAMPLES: usize = 8;

#[derive(Serialize, Default, Debug)]
pub struct Report {
    pub valid: bool,
    pub anomalies: BTreeMap<&'static str, Finding>,
    pub stats: Stats,
}

#[derive(Serialize, Default, Debug)]
pub struct Finding {
    pub count: usize,
    pub examples: Vec<String>,
}

#[derive(Serialize, Default, Debug)]
pub struct Stats {
    /// command -> outcome -> count
    pub ops: BTreeMap<String, BTreeMap<&'static str, usize>>,
    /// fault annotation -> number of ops it touched
    pub faults: BTreeMap<String, usize>,
    /// nemesis action -> count
    pub nemesis: BTreeMap<String, usize>,
    /// key -> entries in the final file
    pub final_entries: BTreeMap<String, usize>,
    /// indeterminate (`info`) appends that did / did not take effect
    pub info_appends_committed: usize,
    pub info_appends_vanished: usize,
    /// temp files left by writers killed mid-write
    pub leftover_temp_files: usize,
    /// longest wait not explained by a paused process or the op's own fault
    pub max_unexplained_wait_s: f64,
    pub duration_s: f64,
}

impl Report {
    pub fn summary(&self) -> String {
        let mut s = String::new();
        let verdict = if self.valid { "VALID" } else { "INVALID" };
        let _ = writeln!(
            s,
            "result: {verdict} ({:.1}s of history)",
            self.stats.duration_s
        );
        let _ = writeln!(s, "operations:");
        for (cmd, outcomes) in &self.stats.ops {
            let counts: Vec<String> = outcomes.iter().map(|(t, n)| format!("{t} {n}")).collect();
            let _ = writeln!(s, "  {cmd:<8} {}", counts.join(", "));
        }
        let _ = writeln!(
            s,
            "indeterminate appends: {} committed, {} vanished",
            self.stats.info_appends_committed, self.stats.info_appends_vanished
        );
        let _ = writeln!(
            s,
            "temp files left by killed writers: {}",
            self.stats.leftover_temp_files
        );
        let _ = writeln!(
            s,
            "longest unexplained wait: {:.2}s",
            self.stats.max_unexplained_wait_s
        );
        if !self.stats.nemesis.is_empty() {
            let _ = writeln!(s, "nemesis: {}", join_counts(&self.stats.nemesis));
        }
        if !self.stats.faults.is_empty() {
            let _ = writeln!(s, "faults on ops: {}", join_counts(&self.stats.faults));
        }
        let finals: Vec<String> = self
            .stats
            .final_entries
            .iter()
            .map(|(k, n)| format!("{k}: {n}"))
            .collect();
        let _ = writeln!(s, "final entries: {}", finals.join(", "));
        if !self.anomalies.is_empty() {
            let _ = writeln!(s, "anomalies:");
            for (kind, finding) in &self.anomalies {
                let _ = writeln!(s, "  {kind} ({}):", finding.count);
                for example in &finding.examples {
                    let _ = writeln!(s, "    - {example}");
                }
                if finding.count > finding.examples.len() {
                    let _ = writeln!(
                        s,
                        "    ... and {} more",
                        finding.count - finding.examples.len()
                    );
                }
            }
        }
        s
    }
}

fn join_counts<K: std::fmt::Display>(m: &BTreeMap<K, usize>) -> String {
    m.iter()
        .map(|(k, n)| format!("{k} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// An invocation paired with its completion.
struct Op<'a> {
    inv: &'a Event,
    done: Option<&'a Event>,
}

impl Op<'_> {
    fn typ(&self) -> Type {
        self.done.map_or(Type::Info, |d| d.typ)
    }
    fn start(&self) -> u64 {
        self.inv.time
    }
    /// Completion is recorded only after the process is reaped, so even for
    /// crashed (`info`) ops this is a hard upper bound on when they acted.
    /// Not so for abandoned ops: the process outlived the client, so it may
    /// have acted at any later time.
    fn end(&self) -> u64 {
        match self.done {
            Some(d) if !self.has_fault("abandoned") => d.time,
            _ => u64::MAX,
        }
    }
    /// How long the caller waited, as opposed to when the op could have acted.
    fn latency(&self) -> Option<u64> {
        self.done.map(|d| d.time - self.inv.time)
    }
    fn key(&self) -> Option<&str> {
        self.inv.key.as_deref()
    }
    fn line(&self) -> Option<&str> {
        match &self.inv.value {
            Value::Entry(line) => Some(line),
            _ => None,
        }
    }
    fn value(&self) -> &Value {
        self.done.map_or(&Value::None, |d| &d.value)
    }
    fn error(&self) -> &str {
        self.done.and_then(|d| d.error.as_deref()).unwrap_or("")
    }
    fn has_fault(&self, name: &str) -> bool {
        let mut all = self
            .inv
            .faults
            .iter()
            .chain(self.done.into_iter().flat_map(|d| &d.faults));
        all.any(|f| f == name)
    }
    /// A short name for the kind of command, for stats and outcome rules.
    fn command(&self) -> &str {
        if self.inv.process == SETUP {
            return "seed";
        }
        match self.inv.f {
            F::Append => "log",
            F::Create => "show-path",
            F::Read => match self.inv.argv.first().map(String::as_str) {
                Some("search") => "search",
                Some("..") => "list",
                _ => "print",
            },
            _ => "?",
        }
    }
    fn describe(&self) -> String {
        let mut s = format!(
            "#{} {} `plan {}` (today={}) -> {}",
            self.inv.index,
            self.inv.process,
            self.inv.argv.join(" "),
            self.inv.mock_date.as_deref().unwrap_or("?"),
            self.typ().as_str(),
        );
        if let Some(d) = self.done {
            if let Some(code) = d.exit {
                let _ = write!(s, " exit={code}");
            }
            if let Some(sig) = d.signal {
                let _ = write!(s, " signal={sig}");
            }
        }
        let end = match self.end() {
            u64::MAX => "?".to_string(),
            t => format!("{:.3}", t as f64 / 1e6),
        };
        let _ = write!(s, " [{:.3}ms..{end}ms]", self.start() as f64 / 1e6);
        s
    }
}

#[derive(Default)]
struct Checker {
    report: Report,
}

/// `stall` bounds how long any caller may wait beyond time spent behind
/// processes the nemesis deliberately paused.
pub fn check(history: &[Event], snap: &Snapshot, stall: Duration) -> Report {
    let mut c = Checker::default();
    let ops = c.pair(history);
    c.stats(history, &ops);
    c.check_outcomes(&ops);
    c.check_stalls(history, &ops, stall.as_nanos() as u64);
    for e in history.iter().filter(|e| e.process == REAPER) {
        if let Value::Text(problem) = &e.value {
            c.anomaly("timeout", problem.clone());
        }
    }
    let finals = c.parse_finals(snap);
    c.check_entries(&ops, &finals);
    c.check_reads_and_order(&ops, &finals, snap);
    c.check_existence(&ops, snap);
    for stray in &snap.stray {
        c.anomaly("stray-file", format!("{stray} left in the plan directory"));
    }
    for op in &ops {
        if let Some(warning) = op.done.and_then(|d| d.warning.as_deref()) {
            c.anomaly(
                "unexpected-files-warning",
                format!("{}: {warning}", op.describe()),
            );
        }
    }
    c.report.stats.leftover_temp_files = snap.temp.len();
    c.report.valid = c.report.anomalies.is_empty();
    c.report
}

impl Checker {
    fn anomaly(&mut self, kind: &'static str, example: String) {
        push(self.report.anomalies.entry(kind).or_default(), example);
    }

    fn pair<'a>(&mut self, history: &'a [Event]) -> Vec<Op<'a>> {
        let mut ops: Vec<Op> = Vec::new();
        let mut pending: HashMap<&str, usize> = HashMap::new();
        for e in history
            .iter()
            .filter(|e| e.process != NEMESIS && e.process != REAPER)
        {
            if e.typ == Type::Invoke {
                if let Some(prev) = pending.insert(&e.process, ops.len()) {
                    let prev = ops[prev].describe();
                    self.anomaly(
                        "harness",
                        format!(
                            "{} invoked #{} while {prev} was pending",
                            e.process, e.index
                        ),
                    );
                }
                ops.push(Op { inv: e, done: None });
            } else if let Some(i) = pending.remove(e.process.as_str()) {
                ops[i].done = Some(e);
            } else {
                self.anomaly(
                    "harness",
                    format!("completion #{} by {} has no invocation", e.index, e.process),
                );
            }
        }
        for &i in pending.values() {
            self.anomaly("incomplete-op", ops[i].describe());
        }
        ops
    }

    fn stats(&mut self, history: &[Event], ops: &[Op]) {
        let stats = &mut self.report.stats;
        for e in history.iter().filter(|e| e.process == NEMESIS) {
            let name = match (&e.f, &e.value) {
                (F::Start | F::Stop, Value::Text(fault)) => format!("{fault} {}", e.f.as_str()),
                (f, _) => f.as_str().to_string(),
            };
            *stats.nemesis.entry(name).or_default() += 1;
        }
        for op in ops {
            let cmd = stats.ops.entry(op.command().to_string()).or_default();
            *cmd.entry(op.typ().as_str()).or_default() += 1;
            let faults = op
                .inv
                .faults
                .iter()
                .chain(op.done.into_iter().flat_map(|d| &d.faults));
            for fault in faults {
                *stats.faults.entry(fault.clone()).or_default() += 1;
            }
        }
        stats.duration_s = history.last().map_or(0.0, |e| e.time as f64 / 1e9);
    }

    /// Every exit status must be explained by the op or a fault we injected.
    fn check_outcomes(&mut self, ops: &[Op]) {
        for op in ops {
            let Some(done) = op.done else { continue };
            if op.has_fault("timeout") {
                self.anomaly("timeout", op.describe());
            }
            if op.inv.process == FINAL && done.typ != Type::Ok {
                self.anomaly(
                    "final-read-failed",
                    format!("{}: {}", op.describe(), op.error()),
                );
            }
            if let Some(sig) = done.signal
                && !(op.has_fault("sigkill") || op.has_fault("timeout"))
            {
                self.anomaly(
                    "crash",
                    format!("{} died from signal {sig} nobody sent", op.describe()),
                );
            }
            match done.exit {
                None | Some(0) => {}
                // plan must never panic, not even when its stdio is broken.
                Some(101) => self.anomaly(
                    "crash",
                    format!("{} panicked: {}", op.describe(), op.error()),
                ),
                Some(2) if op.command() == "print" && op.error().is_empty() => {} // missing file
                // plan may have written its usage error before we closed the pipe.
                Some(2) if op.has_fault("stderr-closed") => {}
                Some(2)
                    if op.error().contains("No plan file for that date")
                        || (op.error().contains("Message cannot be empty")
                            && op.has_fault("stdin-closed")) => {}
                Some(_) => self.anomaly(
                    "unexpected-error",
                    format!("{}: {}", op.describe(), op.error()),
                ),
            }
            if let Value::Malformed(reason) = &done.value {
                self.anomaly("malformed-read", format!("{}: {reason}", op.describe()));
            }
        }
    }

    /// Liveness: one caller's trouble must not become everyone's. Each op's
    /// wait, minus the time some process was deliberately paused (a stopped
    /// lock holder legitimately stalls everyone), must stay under `stall`.
    /// Ops slowed by their own fault are exempt.
    fn check_stalls(&mut self, history: &[Event], ops: &[Op], stall: u64) {
        const OWN: &[&str] = &[
            "sigstop",
            "stdout-hung",
            "stdin-slow",
            "abandoned",
            "timeout",
        ];
        let done_at: HashMap<usize, u64> = ops
            .iter()
            .filter_map(|op| Some((op.inv.index, op.done?.time)))
            .collect();
        let mut paused: Vec<(u64, u64)> = Vec::new();
        for (i, e) in history.iter().enumerate() {
            if e.process != NEMESIS || e.f != F::Pause {
                continue;
            }
            // Until resumed, or until the process ended (killed while stopped,
            // or abandoned, which resumes it).
            let resumed = history[i + 1..]
                .iter()
                .find(|r| r.process == NEMESIS && r.f == F::Resume && r.target == e.target)
                .map(|r| r.time);
            let end = resumed
                .or_else(|| e.target.and_then(|t| done_at.get(&t).copied()))
                .unwrap_or(u64::MAX);
            paused.push((e.time, end));
        }
        for op in ops {
            let Some(wait) = op.latency() else { continue };
            if OWN.iter().any(|f| op.has_fault(f)) {
                continue;
            }
            let (start, end) = (op.start(), op.start() + wait);
            let excused: u64 = paused
                .iter()
                .map(|&(a, b)| b.min(end).saturating_sub(a.max(start)))
                .sum();
            let unexplained = wait.saturating_sub(excused);
            let stats = &mut self.report.stats;
            stats.max_unexplained_wait_s =
                stats.max_unexplained_wait_s.max(unexplained as f64 / 1e9);
            if unexplained > stall {
                self.anomaly(
                    "stall",
                    format!(
                        "{} waited {:.2}s, only {:.2}s of it behind paused processes",
                        op.describe(),
                        wait as f64 / 1e9,
                        excused as f64 / 1e9
                    ),
                );
            }
        }
    }

    fn parse_finals(&mut self, snap: &Snapshot) -> BTreeMap<String, Vec<String>> {
        let mut finals = BTreeMap::new();
        for (key, content) in &snap.files {
            match model::parse_plan(key, content) {
                Ok(entries) => {
                    self.report
                        .stats
                        .final_entries
                        .insert(key.clone(), entries.len());
                    finals.insert(key.clone(), entries);
                }
                Err(reason) => self.anomaly("malformed-file", format!("{key}.plan: {reason}")),
            }
        }
        finals
    }

    /// Every entry in the final state was written exactly once, by an append
    /// that targeted that file and didn't fail; every acknowledged append is there.
    fn check_entries(&mut self, ops: &[Op], finals: &BTreeMap<String, Vec<String>>) {
        let appends: HashMap<&str, &Op> = ops
            .iter()
            .filter(|op| op.inv.f == F::Append)
            .filter_map(|op| Some((op.line()?, op)))
            .collect();
        let mut found: HashMap<&str, Vec<&str>> = HashMap::new();
        for (key, entries) in finals {
            for line in entries {
                found.entry(line).or_default().push(key);
                match appends.get(line.as_str()) {
                    None => self.anomaly(
                        "garbage-entry",
                        format!("{key}.plan contains {line:?}, which no client wrote"),
                    ),
                    Some(op) if op.key() != Some(key) => {
                        self.anomaly(
                            "misplaced-entry",
                            format!("{} landed in {key}.plan", op.describe()),
                        );
                    }
                    Some(_) => {}
                }
            }
        }
        let mut dups: Vec<_> = found.iter().filter(|(_, keys)| keys.len() > 1).collect();
        dups.sort();
        for (line, keys) in dups {
            self.anomaly(
                "duplicate-entry",
                format!("{line:?} appears {} times, in {keys:?}", keys.len()),
            );
        }
        for op in ops.iter().filter(|op| op.inv.f == F::Append) {
            let present = op.line().is_some_and(|l| found.contains_key(l));
            match (op.typ(), present) {
                (Type::Ok, false) => self.anomaly(
                    "lost-write",
                    format!(
                        "{} was acknowledged but is not in the final state",
                        op.describe()
                    ),
                ),
                (Type::Fail, true) => self.anomaly(
                    "failed-write-visible",
                    format!(
                        "{} reported failure but its entry was committed",
                        op.describe()
                    ),
                ),
                (Type::Info, true) => self.report.stats.info_appends_committed += 1,
                (Type::Info, false) => self.report.stats.info_appends_vanished += 1,
                _ => {}
            }
        }
    }

    fn check_reads_and_order(
        &mut self,
        ops: &[Op],
        finals: &BTreeMap<String, Vec<String>>,
        snap: &Snapshot,
    ) {
        let mut keys: BTreeSet<&str> = snap.files.keys().map(String::as_str).collect();
        for op in ops {
            keys.extend(op.key());
            match op.value() {
                Value::Entries(m) => keys.extend(m.keys().map(String::as_str)),
                Value::Counts(m) => keys.extend(m.keys().map(String::as_str)),
                _ => {}
            }
        }
        for key in keys {
            if snap.files.contains_key(key) && !finals.contains_key(key) {
                continue; // final file is malformed; already reported
            }
            let list = finals.get(key).map_or(&[][..], Vec::as_slice);
            self.check_key(key, list, ops);
        }
    }

    /// Check every op that touched `key` against its final inbox, `list`.
    ///
    /// Inserting an entry never moves the others, so every state the inbox
    /// passes through is an ordered subset of the final one, and states only
    /// grow: each read must be a subsequence of `list`, and the reads must nest.
    /// The nesting then pins each entry between two *levels*: an entry first
    /// seen by the reads at level k was committed after level k-1's state, and
    /// no op that finished before another started may sit at a later level.
    /// Entries that share a timestamp stay in commit order, which pins down
    /// their order among themselves too.
    fn check_key(&mut self, key: &str, list: &[String], ops: &[Op]) {
        self.check_sorted(key, "the final file", list);
        let mut index: HashMap<&str, usize> = HashMap::new();
        for (i, line) in list.iter().enumerate() {
            index.entry(line).or_insert(i);
        }

        // Every well-formed read: the final indices it saw, or just a count.
        let mut entry_reads: Vec<(&Op, Vec<usize>)> = Vec::new();
        let mut count_reads: Vec<(&Op, usize)> = Vec::new();
        for op in ops {
            if op.inv.f != F::Read || op.typ() != Type::Ok || op.key().is_some_and(|k| k != key) {
                continue;
            }
            match op.value() {
                Value::Entries(m) => {
                    let obs = m.get(key).map_or(&[][..], Vec::as_slice);
                    self.check_sorted(key, &op.describe(), obs);
                    if let Some(seen) = self.subsequence(key, op, obs, list, &index) {
                        entry_reads.push((op, seen));
                    }
                }
                Value::Counts(m) => {
                    let n = m.get(key).copied().unwrap_or(0);
                    if n > list.len() {
                        self.anomaly(
                            "phantom-read",
                            format!(
                                "{} counted {n} entries in {key}, but only {} were ever committed",
                                op.describe(),
                                list.len()
                            ),
                        );
                    } else {
                        count_reads.push((op, n));
                    }
                }
                _ => {} // malformed: already reported
            }
        }

        // The states reads saw, smallest first, must nest. With the final state
        // on top, they are the levels.
        entry_reads.sort_by_key(|(_, seen)| seen.len());
        let mut levels: Vec<&[usize]> = Vec::new();
        let mut witness: Vec<&Op> = Vec::new();
        for (op, seen) in &entry_reads {
            match levels.last() {
                Some(prev) if prev.len() == seen.len() && *prev == seen.as_slice() => continue,
                Some(prev) if prev.len() == seen.len() || !is_subset(prev, seen) => {
                    let earlier = witness.last().unwrap();
                    self.anomaly(
                        "incomparable-reads",
                        format!(
                            "in {key}, {} and {} each saw entries the other didn't; no order of commits explains both",
                            earlier.describe(),
                            op.describe()
                        ),
                    );
                    return; // levels are meaningless now
                }
                _ => {}
            }
            if !seen.is_empty() {
                levels.push(seen);
                witness.push(op);
            }
        }
        let everything: Vec<usize> = (0..list.len()).collect();
        if levels.last().is_none_or(|top| top.len() < list.len()) {
            levels.push(&everything);
        }
        let sizes: Vec<usize> = levels.iter().map(|l| l.len()).collect();
        // Level 1 is the smallest non-empty state; level 0 is the empty inbox.
        let mut level = vec![0; list.len()];
        for (k, set) in levels.iter().enumerate().rev() {
            for &i in *set {
                level[i] = k + 1;
            }
        }
        // `sizes` strictly increases, so a state's size identifies its level.
        let level_of_size = |n: usize| sizes.partition_point(|&s| s < n);

        let mut points: Vec<(usize, &Op)> = Vec::new();
        let mut ties: BTreeMap<Option<&str>, Vec<(usize, &Op)>> = BTreeMap::new();
        for op in ops {
            if op.inv.f == F::Append
                && op.key() == Some(key)
                && op.typ() != Type::Fail
                && let Some(&i) = op.line().and_then(|l| index.get(l))
            {
                points.push((2 * level[i] - 1, op));
                ties.entry(model::entry_time(&list[i]))
                    .or_default()
                    .push((i, op));
            }
        }
        for (op, seen) in &entry_reads {
            let k = if seen.is_empty() {
                0
            } else {
                level_of_size(seen.len()) + 1
            };
            points.push((2 * k, op));
        }
        for &(op, n) in &count_reads {
            // A count that matches no level was taken partway through a level.
            let k = if n == 0 { 0 } else { level_of_size(n) + 1 };
            let exact = n == 0 || sizes.get(k - 1) == Some(&n);
            points.push((if exact { 2 * k } else { 2 * k - 1 }, op));
        }
        let describe = |pos: usize| match pos {
            0 => "an empty inbox".to_string(),
            p if p % 2 == 0 => format!("a state with {} entries", sizes[p / 2 - 1]),
            p => format!(
                "a change committed by the time it held {} entries",
                sizes[p / 2]
            ),
        };
        self.check_order("linearizability", key, &points, describe);

        // Same-timestamp entries are in commit order, so an entry that became
        // visible later must come later, and so must one invoked later.
        for (_, group) in ties.iter_mut() {
            group.sort_by_key(|(i, _)| *i);
            for pair in group.windows(2) {
                let ((i, _), (j, _)) = (pair[0], pair[1]);
                if level[i] > level[j] {
                    self.anomaly(
                        "tie-order",
                        format!(
                            "in {key}, {:?} and {:?} share a timestamp, so they must stay in commit order, but the second was visible first",
                            list[i], list[j]
                        ),
                    );
                }
            }
            self.check_order("tie-order", key, group, |i| format!("final position {i}"));
        }
    }

    /// The final indices `obs` saw, if it is an ordered subset of `list`.
    fn subsequence(
        &mut self,
        key: &str,
        op: &Op,
        obs: &[String],
        list: &[String],
        index: &HashMap<&str, usize>,
    ) -> Option<Vec<usize>> {
        let mut seen: Vec<usize> = Vec::with_capacity(obs.len());
        for line in obs {
            let Some(&i) = index.get(line.as_str()) else {
                self.anomaly(
                    "phantom-read",
                    format!(
                        "{} saw {line:?} in {key}, which is not in the final state",
                        op.describe()
                    ),
                );
                return None;
            };
            if let Some(&prev) = seen.last()
                && i <= prev
            {
                self.anomaly(
                    "reordered-read",
                    format!(
                        "{} saw {line:?} after {:?} in {key}, but the final file has them the other way round",
                        op.describe(),
                        list[prev]
                    ),
                );
                return None;
            }
            seen.push(i);
        }
        Some(seen)
    }

    /// Timed entries must be in time order (untimed ones are ignored).
    fn check_sorted(&mut self, key: &str, whose: &str, lines: &[String]) {
        let mut timed = lines
            .iter()
            .filter_map(|l| Some((model::entry_time(l)?, l)));
        let Some(mut prev) = timed.next() else { return };
        for cur in timed {
            if cur.0 < prev.0 {
                self.anomaly(
                    "unsorted-inbox",
                    format!("{whose}: {key} has {:?} before {:?}", prev.1, cur.1),
                );
                return;
            }
            prev = cur;
        }
    }

    /// If A finished before B started, A's position must not exceed B's.
    fn check_order(
        &mut self,
        kind: &'static str,
        key: &str,
        points: &[(usize, &Op)],
        describe: impl Fn(usize) -> String,
    ) {
        let mut by_end: Vec<_> = points.iter().collect();
        by_end.sort_by_key(|(_, op)| op.end());
        let mut by_start: Vec<_> = points.iter().collect();
        by_start.sort_by_key(|(_, op)| op.start());
        let mut done = by_end.into_iter().peekable();
        let mut latest: Option<&(usize, &Op)> = None;
        for b in by_start {
            while let Some(a) = done.next_if(|a| a.1.end() < b.1.start()) {
                if latest.is_none_or(|m| a.0 > m.0) {
                    latest = Some(a);
                }
            }
            if let Some(a) = latest
                && a.0 > b.0
            {
                self.anomaly(
                    kind,
                    format!(
                        "{key}: {} finished before {} started, but is ordered after it ({} vs {})",
                        a.1.describe(),
                        b.1.describe(),
                        describe(a.0),
                        describe(b.0),
                    ),
                );
            }
        }
    }

    fn check_existence(&mut self, ops: &[Op], snap: &Snapshot) {
        let mut created: HashMap<&str, &Op> = HashMap::new();
        for op in ops {
            if matches!(op.inv.f, F::Append | F::Create)
                && op.typ() == Type::Ok
                && let Some(key) = op.key()
            {
                let first = created.entry(key).or_insert(op);
                if op.end() < first.end() {
                    *first = op;
                }
            }
            if op.inv.f == F::Create
                && op.typ() == Type::Ok
                && !op.key().is_some_and(|k| snap.files.contains_key(k))
            {
                self.anomaly(
                    "lost-file",
                    format!("{} succeeded but the file does not exist", op.describe()),
                );
            }
        }
        for op in ops {
            if op.typ() == Type::Fail
                && op.error().contains("No plan file for that date")
                && let Some(w) = op.key().and_then(|k| created.get(k))
                && w.end() < op.start()
            {
                self.anomaly(
                    "stale-not-found",
                    format!(
                        "{} reported no file, but {} had already created it",
                        op.describe(),
                        w.describe()
                    ),
                );
            }
        }
    }
}

fn push(finding: &mut Finding, example: String) {
    finding.count += 1;
    if finding.examples.len() < MAX_EXAMPLES {
        finding.examples.push(example);
    }
}

/// Whether sorted `a` is a subset of sorted `b`.
fn is_subset(a: &[usize], b: &[usize]) -> bool {
    let mut b = b.iter().peekable();
    a.iter().all(|x| {
        while b.next_if(|y| *y < x).is_some() {}
        b.next_if_eq(&x).is_some()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::render;

    const K: &str = "2026-02-19";

    /// Hand-built histories. Times are in arbitrary units; events are sorted
    /// by time before checking, like the real recorder would produce.
    #[derive(Default)]
    struct H(Vec<Event>);

    impl H {
        #[allow(clippy::too_many_arguments)]
        fn op(
            &mut self,
            p: &str,
            f: F,
            argv: &str,
            inv_value: Value,
            (t0, t1): (u64, u64),
            typ: Type,
            value: Value,
        ) -> &mut Self {
            let base = Event {
                process: p.into(),
                f,
                key: Some(K.into()),
                mock_date: Some(K.into()),
                ..Event::default()
            };
            self.0.push(Event {
                time: t0,
                typ: Type::Invoke,
                value: inv_value,
                argv: argv.split(' ').map(String::from).collect(),
                ..base.clone()
            });
            let exit = match typ {
                Type::Ok => Some(0),
                Type::Fail => Some(2),
                _ => None,
            };
            let error = (typ == Type::Fail).then(|| "plan: Message cannot be empty.".to_string());
            let (signal, faults) = match typ {
                Type::Info => (Some(9), vec!["sigkill".to_string()]),
                Type::Fail => (None, vec!["stdin-closed".to_string()]),
                _ => (None, vec![]),
            };
            self.0.push(Event {
                time: t1,
                typ,
                value,
                exit,
                error,
                signal,
                faults,
                ..base
            });
            self
        }
        fn append(&mut self, p: &str, line: &str, t: (u64, u64), typ: Type) -> &mut Self {
            let v = Value::Entry(line.into());
            self.op(p, F::Append, "jot x", v.clone(), t, typ, v)
        }
        fn show(&mut self, p: &str, seen: &[&str], t: (u64, u64)) -> &mut Self {
            let seen = seen.iter().map(|s| s.to_string()).collect();
            let v = Value::Entries(BTreeMap::from([(K.to_string(), seen)]));
            self.op(p, F::Read, "show", Value::None, t, Type::Ok, v)
        }
        fn check(&mut self, final_entries: &[&str]) -> Report {
            self.0.sort_by_key(|e| e.time);
            for (i, e) in self.0.iter_mut().enumerate() {
                e.index = i;
            }
            let snap = Snapshot {
                files: BTreeMap::from([(K.to_string(), render(K, final_entries))]),
                temp: vec![],
                stray: vec![],
            };
            check(&self.0, &snap, Duration::from_secs(1))
        }
        fn nemesis(&mut self, f: F, time: u64) -> &mut Self {
            self.0.push(Event {
                process: NEMESIS.into(),
                typ: Type::Info,
                f,
                time,
                ..Event::default()
            });
            self
        }
        /// Mark the most recent completion with `fault`, replacing any others.
        fn fault(&mut self, fault: &str) -> &mut Self {
            let done = self.0.last_mut().unwrap();
            done.faults = vec![fault.into()];
            done.signal = None;
            self
        }
    }

    const SEC: u64 = 1_000_000_000;

    fn kinds(r: &Report) -> Vec<&'static str> {
        r.anomalies.keys().copied().collect()
    }

    #[test]
    fn concurrent_history_is_valid() {
        let r = H::default()
            .append("c0", "a", (0, 10), Type::Ok)
            .append("c1", "b", (1, 12), Type::Ok)
            .show("c2", &["a"], (2, 11)) // overlaps both appends
            .show("c2", &["a", "b"], (13, 14))
            .append("c0", "c", (15, 20), Type::Info) // crashed, committed
            .append("c1", "d", (15, 20), Type::Info) // crashed, vanished
            .append("c1", "e", (21, 22), Type::Fail)
            .check(&["a", "b", "c"]);
        assert!(r.valid, "{}", r.summary());
        assert_eq!(r.stats.info_appends_committed, 1);
        assert_eq!(r.stats.info_appends_vanished, 1);
    }

    #[test]
    fn detects_lost_write() {
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Ok)
            .append("c1", "b", (0, 1), Type::Ok)
            .check(&["a"]);
        assert_eq!(kinds(&r), ["lost-write"]);
    }

    #[test]
    fn detects_failed_write_that_landed() {
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Fail)
            .check(&["a"]);
        assert_eq!(kinds(&r), ["failed-write-visible"]);
    }

    #[test]
    fn detects_duplicate_and_garbage_entries() {
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Ok)
            .check(&["a", "a", "zzz"]);
        assert_eq!(kinds(&r), ["duplicate-entry", "garbage-entry"]);
    }

    #[test]
    fn detects_untimed_entries_seen_out_of_commit_order() {
        // Untimed entries are append-only: seeing `b` without `a` means `b` was
        // committed first, so the final file cannot have `a` before it.
        let r = H::default()
            .append("c0", "a", (0, 10), Type::Ok)
            .append("c1", "b", (0, 10), Type::Ok)
            .show("c2", &["b"], (0, 10))
            .check(&["a", "b"]);
        assert_eq!(kinds(&r), ["tie-order"]);
    }

    #[test]
    fn detects_read_of_vanished_write() {
        let r = H::default()
            .append("c0", "a", (0, 10), Type::Info)
            .show("c1", &["a"], (0, 10))
            .check(&[]);
        assert_eq!(kinds(&r), ["phantom-read"]);
    }

    #[test]
    fn detects_stale_read() {
        // `a` was acknowledged before the read began, yet the read missed it.
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Ok)
            .show("c1", &[], (2, 3))
            .check(&["a"]);
        assert_eq!(kinds(&r), ["linearizability"]);
    }

    #[test]
    fn detects_read_that_goes_backwards() {
        let r = H::default()
            .append("c0", "a", (0, 10), Type::Ok)
            .show("c1", &["a"], (1, 2))
            .show("c2", &[], (3, 4))
            .check(&["a"]);
        assert_eq!(kinds(&r), ["linearizability"]);
    }

    #[test]
    fn detects_appends_committed_out_of_real_time_order() {
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Ok)
            .append("c1", "b", (2, 3), Type::Ok)
            .check(&["b", "a"]);
        assert_eq!(kinds(&r), ["tie-order"]);
    }

    #[test]
    fn crashed_append_bounded_by_reap_time() {
        // An `info` append was reaped at t=1; the process was dead by then,
        // so a read starting at t=2 that misses its committed entry is stale.
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Info)
            .show("c1", &[], (2, 3))
            .check(&["a"]);
        assert_eq!(kinds(&r), ["linearizability"]);
    }

    #[test]
    fn detects_stale_not_found() {
        let mut h = H::default();
        h.append("c0", "a", (0, 1), Type::Ok)
            .append("c1", "b", (2, 3), Type::Fail);
        let failed = h.0.last_mut().unwrap();
        failed.error = Some("plan: No plan file for that date: 2026-02-19.plan".into());
        failed.faults.clear();
        let r = h.check(&["a"]);
        assert_eq!(kinds(&r), ["stale-not-found"]);
    }

    #[test]
    fn detects_unexplained_crash() {
        let mut h = H::default();
        h.append("c0", "a", (0, 1), Type::Ok);
        h.0[1].exit = Some(101);
        h.0[1].typ = Type::Info;
        assert_eq!(kinds(&h.check(&["a"])), ["crash"]);
    }

    #[test]
    fn panic_on_broken_stdio_is_still_a_crash() {
        let mut h = H::default();
        h.append("c0", "a", (0, 1), Type::Ok);
        h.0[0].faults = vec!["stdout-closed".into()];
        h.0[1].exit = Some(101);
        h.0[1].typ = Type::Info;
        assert_eq!(kinds(&h.check(&["a"])), ["crash"]);
    }

    #[test]
    fn detects_unexpected_files_warning() {
        let mut h = H::default();
        h.append("c0", "a", (0, 1), Type::Ok);
        h.0[1].warning = Some("plan: warning: unexpected files in plan directory: x".into());
        assert_eq!(kinds(&h.check(&["a"])), ["unexpected-files-warning"]);
    }

    #[test]
    fn detects_stall() {
        let r = H::default().show("c0", &[], (0, 3 * SEC)).check(&[]);
        assert_eq!(kinds(&r), ["stall"]);
    }

    #[test]
    fn time_behind_a_paused_process_is_not_a_stall() {
        let r = H::default()
            .nemesis(F::Pause, SEC / 10)
            .show("c0", &[], (0, 3 * SEC))
            .nemesis(F::Resume, 2 * SEC + SEC / 2)
            .check(&[]);
        assert!(r.valid, "{}", r.summary());
    }

    #[test]
    fn ops_slowed_by_their_own_fault_are_not_stalls() {
        let r = H::default()
            .show("c0", &[], (0, 3 * SEC))
            .fault("stdout-hung")
            .check(&[]);
        assert!(r.valid, "{}", r.summary());
    }

    #[test]
    fn abandoned_append_may_land_late() {
        // Unlike a crashed op (see `crashed_append_bounded_by_reap_time`), the
        // abandoned process outlived its caller, so it may commit after the read.
        let r = H::default()
            .append("c0", "a", (0, 1), Type::Info)
            .fault("abandoned")
            .show("c1", &[], (2, 3))
            .check(&["a"]);
        assert!(r.valid, "{}", r.summary());
    }

    #[test]
    fn late_stamped_entry_may_land_before_seen_ones() {
        // `a` was stamped earlier but committed later: plan inserts it in time
        // order, ahead of `b`, which a read had already seen. That's fine.
        let r = H::default()
            .append("c0", "10:05 b", (0, 1), Type::Ok)
            .show("c1", &["10:05 b"], (2, 3))
            .append("c2", "10:00 a", (4, 5), Type::Ok)
            .show("c1", &["10:00 a", "10:05 b"], (6, 7))
            .check(&["10:00 a", "10:05 b"]);
        assert!(r.valid, "{}", r.summary());
    }

    #[test]
    fn detects_same_minute_entries_out_of_commit_order() {
        let r = H::default()
            .append("c0", "10:00 a", (0, 1), Type::Ok)
            .append("c1", "10:00 b", (2, 3), Type::Ok)
            .check(&["10:00 b", "10:00 a"]);
        assert_eq!(kinds(&r), ["tie-order"]);
    }

    #[test]
    fn detects_reads_and_files_out_of_time_order() {
        let r = H::default()
            .append("c0", "10:00 a", (0, 10), Type::Ok)
            .append("c1", "10:05 b", (0, 10), Type::Ok)
            .show("c2", &["10:05 b", "10:00 a"], (0, 10))
            .check(&["10:00 a", "10:05 b"]);
        assert_eq!(kinds(&r), ["reordered-read", "unsorted-inbox"]);
        let r = H::default()
            .append("c0", "10:05 b", (0, 1), Type::Ok)
            .append("c1", "10:00 a", (2, 3), Type::Ok)
            .check(&["10:05 b", "10:00 a"]);
        assert_eq!(kinds(&r), ["unsorted-inbox"]);
    }

    #[test]
    fn detects_reads_that_do_not_nest() {
        let r = H::default()
            .append("c0", "10:00 a", (0, 10), Type::Ok)
            .append("c1", "10:05 b", (0, 10), Type::Ok)
            .show("c2", &["10:00 a"], (0, 10))
            .show("c3", &["10:05 b"], (0, 10))
            .check(&["10:00 a", "10:05 b"]);
        assert_eq!(kinds(&r), ["incomparable-reads"]);
    }
}
