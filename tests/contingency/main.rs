#![cfg(all(unix, feature = "test-clock"))]
//! Contingency: does `plan` have a contingency for everything that can go
//! wrong while it is being called concurrently?
//!
//! A run builds a [sandbox], starts N concurrent [client]s that hammer the real
//! `plan` binary with `log`/`jot`/`show`/`search`/`ls`/`--path`, while a
//! [nemesis] kills, pauses, and sabotages processes. Everything that happens is
//! recorded to a [history], and once the dust settles the [checker] verifies the
//! history against plan's concurrency model (an append-only, linearizable inbox
//! per plan file).
//!
//! ```sh
//! cargo test --features test-clock --test contingency -- --nocapture
//!
//! # longer, louder, reproducible
//! CONTINGENCY_TIME=60 CONTINGENCY_CLIENTS=16 CONTINGENCY_SEED=42 \
//!   cargo test --features test-clock --test contingency survives_faults -- --nocapture
//!
//! # re-run only the checker over a previous run's history
//! CONTINGENCY_RECHECK=target/tmp/contingency/<run> \
//!   cargo test --features test-clock --test contingency recheck -- --ignored --nocapture
//! ```
//!
//! | variable                 | default   | meaning                                    |
//! |--------------------------|-----------|--------------------------------------------|
//! | `CONTINGENCY_SEED`       | random    | seeds the workload and nemesis (printed)   |
//! | `CONTINGENCY_TIME`       | `5`       | seconds of workload                        |
//! | `CONTINGENCY_CLIENTS`    | `8`       | concurrent callers                         |
//! | `CONTINGENCY_FAULTS`     | `default` | e.g. `kill,pause`, `none`, `all`           |
//! | `CONTINGENCY_OP_TIMEOUT` | `10`      | seconds before a client gives up on a call |
//! | `CONTINGENCY_STALL`      | `1.5`     | max seconds a caller may wait, not counting |
//! |                          |           | time behind deliberately paused processes  |
//! | `CONTINGENCY_OUT`        | `target/tmp/contingency` | where run directories go    |
//!
//! Each run leaves `history.jsonl`, `results.json`, `results.txt`, and the
//! sandbox (with the final plan files) in its run directory.

mod checker;
mod client;
mod history;
mod model;
mod nemesis;
mod sandbox;

use client::{Client, Ctx, Reapers};
use history::{Event, F, Recorder, Type};
use nemesis::{Control, Fault};
use sandbox::Sandbox;
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Config {
    name: &'static str,
    seed: u64,
    clients: usize,
    duration: Duration,
    faults: Vec<Fault>,
    timeout: Duration,
    stall: Duration,
    out: PathBuf,
}

fn stall_limit() -> Duration {
    Duration::from_secs_f64(env("CONTINGENCY_STALL").unwrap_or(1.5))
}

fn env<T: FromStr>(key: &str) -> Option<T> {
    let raw = std::env::var(key).ok().filter(|v| !v.trim().is_empty())?;
    Some(
        raw.trim()
            .parse()
            .unwrap_or_else(|_| panic!("{key}: can't parse {raw:?}")),
    )
}

impl Config {
    fn from_env(name: &'static str) -> Self {
        Self {
            name,
            seed: env("CONTINGENCY_SEED").unwrap_or_else(rand::random),
            clients: env("CONTINGENCY_CLIENTS").unwrap_or(8),
            duration: Duration::from_secs_f64(env("CONTINGENCY_TIME").unwrap_or(5.0)),
            faults: env::<String>("CONTINGENCY_FAULTS")
                .map(|s| Fault::parse_list(&s))
                .unwrap_or_else(|| Fault::DEFAULT.to_vec()),
            timeout: Duration::from_secs_f64(env("CONTINGENCY_OP_TIMEOUT").unwrap_or(10.0)),
            stall: stall_limit(),
            out: env("CONTINGENCY_OUT")
                .unwrap_or_else(|| Path::new(env!("CARGO_TARGET_TMPDIR")).join("contingency")),
        }
    }

    fn describe(&self) -> String {
        let faults: Vec<&str> = self.faults.iter().map(|f| f.name()).collect();
        format!(
            "{}: seed={} clients={} time={:?} faults=[{}]",
            self.name,
            self.seed,
            self.clients,
            self.duration,
            faults.join(",")
        )
    }
}

struct Outcome {
    report: checker::Report,
    dir: PathBuf,
}

fn run(cfg: &Config) -> Outcome {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let dir = cfg.out.join(format!("{}-{}-{stamp}", cfg.name, cfg.seed));
    let sandbox = Sandbox::create(&dir.join("sandbox")).expect("create sandbox");
    println!("{}\nrun directory: {}", cfg.describe(), dir.display());

    let rec = Recorder::new();
    seed(&sandbox, &rec);
    let ctl = Control::new(cfg.clients + 1); // +1: the final reader
    let reapers = Reapers::default();
    let ctx = Ctx {
        plan_bin: Path::new(env!("CARGO_BIN_EXE_plan")),
        sandbox: &sandbox,
        rec: &rec,
        ctl: &ctl,
        reapers: &reapers,
        base_date: model::base_date(),
        timeout: cfg.timeout,
    };

    let deadline = Instant::now() + cfg.duration;
    thread::scope(|s| {
        for id in 0..cfg.clients {
            let ctx = &ctx;
            let seed = cfg
                .seed
                .wrapping_add((id as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
            s.spawn(move || Client::new(id, format!("c{id}"), seed, ctx).run_until_stopped());
        }
        nemesis::run(
            &ctl,
            &rec,
            &sandbox,
            &cfg.faults,
            cfg.seed ^ 0xA5A5_A5A5,
            deadline,
        );
        ctl.stop();
    });

    // Abandoned processes may still be running; their late writes must land
    // before the final reads, or the final state would be a moving target.
    let abandoned = std::mem::take(&mut *reapers.lock().unwrap_or_else(|e| e.into_inner()));
    for reaper in abandoned {
        if let Some(problem) = reaper.join().expect("reaper panicked") {
            rec.record(Event {
                process: history::REAPER.into(),
                typ: Type::Info,
                value: history::Value::Text(problem),
                ..Event::default()
            });
        }
    }

    // Quiescence: every process is dead and all faults are healed. A final
    // round of reads must succeed and agree with the files on disk.
    let keys: BTreeSet<String> = rec.events().into_iter().filter_map(|e| e.key).collect();
    Client::new(cfg.clients, history::FINAL.into(), cfg.seed, &ctx).final_reads(&keys);

    let events = rec.events();
    history::write_jsonl(&dir.join("history.jsonl"), &events).expect("write history");
    let snapshot = sandbox.snapshot().expect("snapshot sandbox");
    let report = checker::check(&events, &snapshot, cfg.stall);
    save(&dir, &report, &cfg.describe());
    println!("{}", report.summary());
    Outcome { report, dir }
}

/// Create the seeded plan files (with ballast) before any client starts, and
/// record each as a completed create so the checker knows they exist.
fn seed(sandbox: &Sandbox, rec: &Recorder) {
    for key in model::seeded_keys() {
        let op = Event {
            process: history::SETUP.into(),
            f: F::Create,
            key: Some(key.clone()),
            argv: vec!["seed".into()],
            mock_date: Some(key.clone()),
            ..Event::default()
        };
        rec.record(Event {
            typ: Type::Invoke,
            ..op.clone()
        });
        fs::write(
            sandbox.plans.join(format!("{key}.plan")),
            model::render(&key, &[]),
        )
        .expect("seed plan file");
        rec.record(Event {
            typ: Type::Ok,
            ..op
        });
    }
}

fn save(dir: &Path, report: &checker::Report, header: &str) {
    fs::write(
        dir.join("results.json"),
        serde_json::to_string_pretty(report).unwrap(),
    )
    .expect("write results.json");
    fs::write(
        dir.join("results.txt"),
        format!("{header}\n{}", report.summary()),
    )
    .expect("write results.txt");
}

fn assert_valid(cfg: &Config) {
    let out = run(cfg);
    assert!(
        out.report.valid,
        "{}\n{}\nhistory and sandbox kept in {}",
        cfg.describe(),
        out.report.summary(),
        out.dir.display()
    );
}

/// The main event: plan under every default fault at once must uphold its model.
#[test]
fn survives_faults() {
    assert_valid(&Config::from_env("faults"));
}

/// A reader that stops reading (`plan show | less`, pager left open) must
/// not block anyone else. Faults mask each other in the combined run, so this
/// property gets a run of its own.
#[test]
fn survives_hung_readers() {
    let mut cfg = Config::from_env("hung-readers");
    cfg.faults = vec![Fault::HangStdout];
    cfg.duration = cfg.duration.min(Duration::from_secs(3));
    assert_valid(&cfg);
}

/// A harness that never fails proves nothing. A rogue writer that rewrites plan
/// files from stale copies without taking the lock (an uncoordinated editor or
/// sync tool) must show up as lost updates.
#[test]
fn detects_rogue_writer() {
    let mut cfg = Config::from_env("rogue-writer");
    cfg.faults = vec![Fault::RogueWriter];
    cfg.duration = cfg.duration.min(Duration::from_secs(3));
    let out = run(&cfg);
    let anomalies: Vec<_> = out.report.anomalies.keys().collect();
    assert!(
        anomalies
            .iter()
            .any(|k| ["lost-write", "linearizability", "non-prefix-read"].contains(k)),
        "expected the rogue writer to cause lost updates, got {anomalies:?}\n{}\nrun kept in {}",
        out.report.summary(),
        out.dir.display()
    );
}

/// Re-check a stored run (`CONTINGENCY_RECHECK=<run dir>`) without re-running it.
#[test]
#[ignore = "set CONTINGENCY_RECHECK=<run dir> and pass --ignored"]
fn recheck() {
    let dir: PathBuf =
        env("CONTINGENCY_RECHECK").expect("set CONTINGENCY_RECHECK to a run directory");
    let events = history::read_jsonl(&dir.join("history.jsonl")).expect("read history");
    let snapshot = Sandbox::open(&dir.join("sandbox"))
        .snapshot()
        .expect("snapshot sandbox");
    let report = checker::check(&events, &snapshot, stall_limit());
    println!("{}", report.summary());
    assert!(report.valid, "{}", report.summary());
}
