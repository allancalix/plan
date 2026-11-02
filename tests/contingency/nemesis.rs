//! The nemesis: injects faults into the running workload.
//!
//! | fault          | what it does                                                         |
//! |----------------|----------------------------------------------------------------------|
//! | `kill`         | SIGKILL an in-flight `plan` process (mid-write, mid-read, waiting...) |
//! | `pause`        | SIGSTOP an in-flight process for up to 0.5s, then SIGCONT. A paused  |
//! |                | lock holder stalls every other writer: a hung process.               |
//! | `pipe`         | for a window, sabotage new processes' stdio: close stdout/stderr     |
//! |                | before they write (EPIPE), or close/truncate stdin for `log -`.      |
//! | `clock`        | for a window, skew each client's clock by -1/0/+1 days, so clients   |
//! |                | disagree about what "today" is (midnight rollover).                  |
//! | `rogue-writer` | rewrite a plan file from a stale copy without taking the lock, like  |
//! |                | an uncoordinated editor or sync tool. Expected to lose updates; it   |
//! |                | is not in the default set and exists to prove the checker has teeth. |
//!
//! The nemesis never signals a PID directly. It queues a request on the target
//! client's slot, and the client thread, which owns the child and is the only
//! one that reaps it, delivers the signal only while the child is unreaped.
//! That rules out signalling a recycled PID.

use crate::history::{Event, F, NEMESIS, Recorder, Type, Value};
use crate::sandbox::Sandbox;
use rand::rngs::StdRng;
use rand::seq::IndexedRandom;
use rand::{RngExt, SeedableRng};
use std::fs;
use std::ops::Range;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Fault {
    Kill,
    Pause,
    Pipe,
    Clock,
    RogueWriter,
}

impl Fault {
    /// Faults plan is expected to survive with its guarantees intact.
    pub const DEFAULT: &[Fault] = &[Fault::Kill, Fault::Pause, Fault::Pipe, Fault::Clock];

    pub fn name(self) -> &'static str {
        match self {
            Fault::Kill => "kill",
            Fault::Pause => "pause",
            Fault::Pipe => "pipe",
            Fault::Clock => "clock",
            Fault::RogueWriter => "rogue-writer",
        }
    }

    pub fn parse_list(spec: &str) -> Vec<Fault> {
        let mut out = Vec::new();
        for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match part {
                "none" => {}
                "default" => out.extend_from_slice(Self::DEFAULT),
                "all" => {
                    out.extend_from_slice(Self::DEFAULT);
                    out.push(Fault::RogueWriter);
                }
                "kill" => out.push(Fault::Kill),
                "pause" => out.push(Fault::Pause),
                "pipe" => out.push(Fault::Pipe),
                "clock" => out.push(Fault::Clock),
                "rogue-writer" => out.push(Fault::RogueWriter),
                other => panic!(
                    "unknown nemesis fault {other:?}; expected a comma-separated list of: \
                     none, default, all, kill, pause, pipe, clock, rogue-writer"
                ),
            }
        }
        out.sort();
        out.dedup();
        out
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Sig {
    Kill,
    Stop,
    Cont,
}

impl Sig {
    pub fn raw(self) -> libc::c_int {
        match self {
            Sig::Kill => libc::SIGKILL,
            Sig::Stop => libc::SIGSTOP,
            Sig::Cont => libc::SIGCONT,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Sig::Kill => "sigkill",
            Sig::Stop => "sigstop",
            Sig::Cont => "sigcont",
        }
    }
}

#[derive(Default)]
struct Slot {
    /// History index of the invocation currently running, if any.
    op: Option<usize>,
    pending: Vec<Sig>,
}

/// State shared between the clients and the nemesis.
pub struct Control {
    slots: Vec<Mutex<Slot>>,
    skew: Vec<AtomicI64>,
    pipe: AtomicBool,
    stop: AtomicBool,
}

impl Control {
    pub fn new(clients: usize) -> Self {
        Self {
            slots: (0..clients).map(|_| Mutex::default()).collect(),
            skew: (0..clients).map(|_| AtomicI64::new(0)).collect(),
            pipe: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        }
    }

    fn slot(&self, client: usize) -> std::sync::MutexGuard<'_, Slot> {
        self.slots[client].lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn begin(&self, client: usize, op: usize) {
        let mut slot = self.slot(client);
        slot.op = Some(op);
        slot.pending.clear();
    }

    pub fn end(&self, client: usize) {
        let mut slot = self.slot(client);
        slot.op = None;
        slot.pending.clear();
    }

    pub fn take_signals(&self, client: usize) -> Vec<Sig> {
        std::mem::take(&mut self.slot(client).pending)
    }

    /// Ask `client` to signal its process, but only if it is still running `op`.
    fn request(&self, client: usize, op: usize, sig: Sig) -> bool {
        let mut slot = self.slot(client);
        if slot.op == Some(op) {
            slot.pending.push(sig);
            true
        } else {
            false
        }
    }

    fn busy(&self) -> Vec<(usize, usize)> {
        (0..self.slots.len())
            .filter_map(|c| self.slot(c).op.map(|op| (c, op)))
            .collect()
    }

    pub fn skew(&self, client: usize) -> i64 {
        self.skew[client].load(Ordering::SeqCst)
    }

    pub fn pipe_active(&self) -> bool {
        self.pipe.load(Ordering::SeqCst)
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

/// Inject faults until `deadline`, then heal everything. Each fault runs on
/// its own schedule, so kills keep landing while a pipe or clock window is open.
pub fn run(
    ctl: &Control,
    rec: &Recorder,
    sandbox: &Sandbox,
    faults: &[Fault],
    seed: u64,
    deadline: Instant,
) {
    if faults.is_empty() {
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        return;
    }
    thread::scope(|s| {
        for (i, &fault) in faults.iter().enumerate() {
            let rng = StdRng::seed_from_u64(seed.wrapping_add(i as u64));
            s.spawn(move || {
                Nemesis {
                    ctl,
                    rec,
                    sandbox,
                    fault,
                    rng,
                }
                .run(deadline)
            });
        }
    });
}

struct Nemesis<'a> {
    ctl: &'a Control,
    rec: &'a Recorder,
    sandbox: &'a Sandbox,
    fault: Fault,
    rng: StdRng,
}

impl Nemesis<'_> {
    fn run(&mut self, deadline: Instant) {
        while Instant::now() < deadline {
            self.sleep_ms(10..150);
            match self.fault {
                Fault::Kill => self.kill(),
                Fault::Pause => self.pause(),
                Fault::Pipe => self.pipe(),
                Fault::Clock => self.clock(),
                Fault::RogueWriter => self.rogue_write(),
            }
        }
        self.heal();
    }

    fn log(&self, f: F, text: String) {
        self.rec.record(Event {
            process: NEMESIS.into(),
            typ: Type::Info,
            f,
            value: Value::Text(text),
            ..Event::default()
        });
    }

    fn sleep_ms(&mut self, range: Range<u64>) {
        thread::sleep(Duration::from_millis(self.rng.random_range(range)));
    }

    fn pick_busy(&mut self) -> Option<(usize, usize)> {
        self.ctl.busy().choose(&mut self.rng).copied()
    }

    fn kill(&mut self) {
        if let Some((client, op)) = self.pick_busy()
            && self.ctl.request(client, op, Sig::Kill)
        {
            self.log(F::Kill, format!("sigkill c{client} (op #{op})"));
        }
    }

    fn pause(&mut self) {
        let Some((client, op)) = self.pick_busy() else {
            return;
        };
        if !self.ctl.request(client, op, Sig::Stop) {
            return;
        }
        self.log(F::Pause, format!("sigstop c{client} (op #{op})"));
        self.sleep_ms(50..500);
        // Always resume, even past the deadline; if the op already ended
        // (e.g. the client timed it out), there is nothing left to resume.
        if self.ctl.request(client, op, Sig::Cont) {
            self.log(F::Resume, format!("sigcont c{client} (op #{op})"));
        }
    }

    fn pipe(&mut self) {
        self.ctl.pipe.store(true, Ordering::SeqCst);
        self.log(F::PipeStart, "sabotaging stdio of new processes".into());
        self.sleep_ms(100..600);
        self.ctl.pipe.store(false, Ordering::SeqCst);
        self.log(F::PipeStop, "stdio healed".into());
    }

    fn clock(&mut self) {
        let mut desc = Vec::new();
        for (client, skew) in self.ctl.skew.iter().enumerate() {
            let days = self.rng.random_range(-1..=1);
            skew.store(days, Ordering::SeqCst);
            desc.push(format!("c{client}:{days:+}"));
        }
        self.log(F::ClockSkew, desc.join(" "));
        self.sleep_ms(200..1000);
        self.reset_clocks();
        self.log(F::ClockReset, "all clocks at base date".into());
    }

    fn reset_clocks(&self) {
        for skew in &self.ctl.skew {
            skew.store(0, Ordering::SeqCst);
        }
    }

    /// Read a plan file, dawdle, and write the stale copy back. It uses
    /// tmp+rename, so readers never see a torn file; it only loses updates.
    fn rogue_write(&mut self) {
        let files: Vec<_> = fs::read_dir(&self.sandbox.plans)
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "plan"))
            .collect();
        let Some(path) = files.choose(&mut self.rng).cloned() else {
            return;
        };
        let Ok(stale) = fs::read_to_string(&path) else {
            return;
        };
        self.sleep_ms(1..30);
        let tmp = self.sandbox.root.join("rogue-writer.tmp");
        if fs::write(&tmp, &stale).is_ok() && fs::rename(&tmp, &path).is_ok() {
            let name = path.file_name().unwrap().to_string_lossy();
            self.log(F::RogueWrite, format!("rewrote {name} from a stale copy"));
        }
    }

    fn heal(&self) {
        self.ctl.pipe.store(false, Ordering::SeqCst);
        self.reset_clocks();
    }
}
