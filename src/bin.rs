use anyhow::{Context, Result, bail};
use jiff::civil::Date;
use plan::config;
use plan::date;
use plan::file;
use plan::list;
use plan::select::{self, Resolved, Selector};

use clap::Parser;
use std::env;
use std::fs;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;

#[derive(Debug)]
enum PlanError {
    Usage(String),
    SilentExit(i32),
}

impl std::fmt::Display for PlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlanError::Usage(msg) => write!(f, "plan: {}", msg),
            PlanError::SilentExit(_) => write!(f, "silent exit"),
        }
    }
}
impl std::error::Error for PlanError {}

fn usage_err(msg: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(PlanError::Usage(msg.into()))
}

fn silent_exit(code: i32) -> anyhow::Error {
    anyhow::Error::new(PlanError::SilentExit(code))
}

/// Map a failed write to stdout. A reader that goes away early (`plan | head`)
/// is a normal way for a pipeline to end, so exit quietly instead of panicking.
fn stdout_err(e: io::Error) -> anyhow::Error {
    if e.kind() == io::ErrorKind::BrokenPipe {
        silent_exit(0)
    } else {
        anyhow::Error::new(e).context("Error writing to stdout")
    }
}

/// How many plans a terminal list shows before pointing at the rest.
const LIST_LIMIT: usize = 30;

#[derive(Parser, Debug)]
#[command(
    version,
    about = "A standalone tool for writing and managing daily plan files.",
    long_about = None,
    override_usage = "plan [OPTIONS] [SELECTOR]\n       plan [OPTIONS] [SELECTOR] -m <MESSAGE>\n       plan [OPTIONS] search <QUERY...> [--in <SELECTOR>]\n       plan init <DIR>",
    after_help = "Selectors:\n  @, today, yesterday, @~N, \"N days ago\"  one day\n  @-                                     the most recent plan\n  2026-11-01                             that date\n  11-01, 01                              every plan on that day of any year, or of any month\n  2026-11, 2026                          every plan in that month, or that year\n  A..B, A.., ..B, ..                     every plan from A through B\n\nA selector that matches several plans lists them (the newest 30 in a terminal,\nwith the shortest selector for each highlighted). One plan opens in\n$VISUAL/$EDITOR, or prints when stdout is piped.\n\nCommands:\n  search <QUERY>  Search plans (case-insensitive substring); --in narrows it\n  init <DIR>      Configure the plan directory, creating it if needed\n\nExamples:\n  plan                             open today's plan\n  plan ..                          list recent plans\n  plan yesterday | bat             print yesterday's plan (printing never creates one)\n  plan -m \"ship the thing\"         log an entry into today's inbox\n  plan yesterday -m \"forgot this\"  log into yesterday's plan\n  echo note | plan @- -m -         log a line from stdin into the most recent plan\n  plan search deploy --in @~6..    search the last week's plans"
)]
struct Cli {
    /// Override the configured plan directory
    #[arg(long, value_name = "DIR")]
    dir: Option<String>,

    /// Print the plan file's path instead of opening it (creates today's if needed)
    #[arg(long)]
    show_path: bool,

    /// Log MESSAGE into the selected plan's inbox ('-' reads a line from stdin)
    #[arg(short, long, value_name = "MESSAGE", allow_hyphen_values = true)]
    message: Option<String>,

    /// Search only the plans SELECTOR picks (with `plan search`)
    #[arg(long = "in", value_name = "SELECTOR")]
    within: Option<String>,

    /// A selector (today's plan if omitted), or a command: search, init
    #[arg(value_name = "SELECTOR")]
    args: Vec<String>,
}

/// What the positional words ask for.
enum Invocation {
    Init(String),
    Search(String),
    Plan { selector: Option<String> },
}

fn parse_invocation(args: &[String], message: Option<&str>) -> Result<Invocation> {
    let Some((first, rest)) = args.split_first() else {
        return Ok(Invocation::Plan { selector: None });
    };
    let first = first.as_str();
    if let Some(hint) = removed_command_hint(first) {
        return Err(usage_err(hint));
    }
    let command = match (first, rest) {
        ("init", [dir]) => Some(Invocation::Init(dir.clone())),
        ("init", _) => return Err(usage_err("usage: plan init <DIR>")),
        ("search", []) => return Err(usage_err("usage: plan search <QUERY>")),
        ("search", words) => Some(Invocation::Search(words.join(" "))),
        _ => None,
    };
    if let Some(command) = command {
        if message.is_some() {
            return Err(usage_err(format!("-m can't be used with `plan {first}`.")));
        }
        return Ok(command);
    }
    let is_selector = select::parse(first).is_ok();
    // A mistyped selector is a typo, not a message.
    if !is_selector && !looks_like_words(first) && !rest.is_empty() {
        let e = select::parse(first).unwrap_err();
        return Err(usage_err(e.to_string()));
    }
    // A selector takes nothing after it, so leftover words are most likely an
    // unquoted message: suggest the quoted form.
    let quoted = |words: &[String]| {
        let all: Vec<&str> = message
            .into_iter()
            .chain(words.iter().map(String::as_str))
            .collect();
        format!("{:?}", all.join(" "))
    };
    let error = match (message, is_selector, rest.first()) {
        // `plan yesterday`, or `plan ship`, which resolving reports.
        (_, true, None) | (None, false, None) => {
            return Ok(Invocation::Plan {
                selector: Some(first.to_string()),
            });
        }
        // `plan today -m fix the build`
        (Some(_), true, Some(extra)) => format!(
            "unexpected argument {extra:?}. To log a message of several words, quote it: plan {first} -m {}",
            quoted(rest)
        ),
        // `plan -m fix the build`: everything left over is message.
        (Some(_), false, _) => format!(
            "unexpected argument {first:?}. To log a message of several words, quote it: plan -m {}",
            quoted(args)
        ),
        // `plan yesterday forgot this`
        (None, true, Some(extra)) => format!(
            "unexpected argument {extra:?}. To log a message, use -m: plan {first} -m {}",
            quoted(rest)
        ),
        // `plan ship the thing`
        (None, false, Some(_)) => format!(
            "{first:?} is not a selector. To log a message, use -m: plan -m {}",
            quoted(args)
        ),
    };
    Err(usage_err(error))
}

/// Whether a word that isn't a selector reads like prose rather than a typo
/// of one (`@~x`, `13-01`, `2026-1-1..`).
fn looks_like_words(s: &str) -> bool {
    let s = s.trim_start();
    !s.starts_with('@')
        && s.chars()
            .any(|c| !(c.is_ascii_digit() || c == '-' || c == '.'))
}

fn read_stdin_line() -> io::Result<String> {
    use std::io::BufRead;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn open_editor(path: &Path) -> Result<()> {
    let editor_env = env::var("VISUAL")
        .or_else(|_| env::var("EDITOR"))
        .unwrap_or_else(|_| "nano".to_string());

    let args = shlex::split(&editor_env).unwrap_or_else(|| vec![editor_env.clone()]);
    if args.is_empty() {
        bail!("Invalid editor specified: {}", editor_env);
    }

    let mut cmd = ProcessCommand::new(&args[0]);
    cmd.args(&args[1..]).arg(path);

    let status = cmd
        .status()
        .context(format!("Failed to launch editor '{}'", args[0]))?;

    if !status.success() {
        if let Some(code) = status.code() {
            return Err(silent_exit(code));
        } else {
            bail!("Editor terminated by signal");
        }
    }

    Ok(())
}

/// Whether stdout is a terminal: `plan` opens the editor there, and prints
/// the plan when piped. Tests can force either with `PLAN_MOCK_TTY=1|0`.
fn stdout_is_terminal() -> bool {
    #[cfg(feature = "test-clock")]
    if let Ok(mock) = env::var("PLAN_MOCK_TTY") {
        return mock == "1";
    }
    io::stdout().is_terminal()
}

fn maybe_warn_unexpected(cfg: &config::Config, unexpected: &[String]) {
    if cfg.scan.warn_unexpected {
        file::warn_unexpected_files(unexpected);
    }
}

/// Commands that were folded into selectors, with what to type now.
fn removed_command_hint(word: &str) -> Option<&'static str> {
    match word {
        "log" | "jot" => {
            Some("`plan log` is now `plan [selector] -m <message>`, e.g. `plan -m \"ship it\"`")
        }
        "show" => Some(
            "`plan show` is now `plan <selector>` with stdout piped, e.g. `plan yesterday | bat`",
        ),
        "ls" => Some(
            "`plan ls` is now `plan ..`, which lists your most recent plans (or a range: `plan @~6..`)",
        ),
        _ => None,
    }
}

fn init(dir: &str) -> Result<()> {
    let expanded_dir = config::expand_tilde(dir);
    if !expanded_dir.exists() {
        fs::create_dir_all(&expanded_dir).context(format!(
            "Error creating directory {}",
            expanded_dir.display()
        ))?;
    }
    config::Config::init(dir)?;
    writeln!(io::stdout(), "Configured plan directory: {}", dir).map_err(stdout_err)?;
    Ok(())
}

/// The dates of the plan files in the directory.
fn plan_dates(entries: &[fs::DirEntry]) -> Vec<Date> {
    entries
        .iter()
        .filter_map(|e| {
            let name = e.file_name();
            let stem = name.to_str()?.strip_suffix(".plan")?;
            Date::strptime("%Y-%m-%d", stem).ok()
        })
        .collect()
}

fn selector_err(e: select::Error) -> anyhow::Error {
    usage_err(e.to_string())
}

/// List rows for `plans`, counting each one's inbox entries.
fn rows(dir: &Path, plans: &[Date]) -> Vec<list::Row> {
    plans
        .iter()
        .map(|&d| list::Row {
            date: d,
            entries: fs::read_to_string(date::get_plan_path(dir, d))
                .map(|c| file::inbox_entries(&c))
                .unwrap_or(0),
        })
        .collect()
}

/// What `plan` does with one plan: log into it, print its path, open it, or
/// print it. Only today's plan is ever created: plan never invents history.
fn act_on(cli: &Cli, dir: &Path, day: Date, today: Date) -> Result<()> {
    let path = date::get_plan_path(dir, day);
    let ensure = |guard: &file::LockGuard| -> Result<()> {
        match date::ensure_file_exists(&path, day, day != today, guard) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Err(usage_err(format!(
                "No plan file for that date: {}",
                date::format_filename(day)
            ))),
            other => other.context("Error ensuring file exists"),
        }
    };
    if let Some(message) = &cli.message {
        let text = if message == "-" {
            read_stdin_line()?
        } else {
            message.trim().to_string()
        };
        if text.is_empty() {
            return Err(usage_err("Message cannot be empty."));
        }
        let lock = file::acquire_lock(&path)?;
        ensure(&lock)?;
        // Stamp the entry under the lock, so entries committed one after
        // another are never stamped out of order.
        let entry = format!("{} {}", date::entry_time(), text);
        file::insert_into_inbox(&path, &entry, &lock)?;
    } else if cli.show_path || stdout_is_terminal() {
        ensure(&file::acquire_lock(&path)?)?;
        if cli.show_path {
            writeln!(io::stdout(), "{}", path.display()).map_err(stdout_err)?;
        } else {
            open_editor(&path)?;
        }
    } else {
        // Piped (`plan yesterday | bat`): print the plan. Never creates one.
        if !path.exists() {
            return Err(silent_exit(2));
        }
        let content = {
            let _lock = file::acquire_shared_lock(&path)?;
            fs::read_to_string(&path)?
        };
        // Print only after releasing the lock: stdout may be a pipe nobody
        // is reading (a pager left open), and every writer would wait on it.
        write!(io::stdout(), "{}", content).map_err(stdout_err)?;
    }
    Ok(())
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let invocation = parse_invocation(&cli.args, cli.message.as_deref())?;

    if let Invocation::Init(dir) = &invocation {
        return init(dir);
    }

    let mut cfg = config::Config::load()?;

    if let Some(dir) = &cli.dir {
        cfg.dir = config::expand_tilde(dir);
        if !cfg.dir.exists() {
            fs::create_dir_all(&cfg.dir)
                .context(format!("Error creating directory {}", cfg.dir.display()))?;
        }
    }

    if cli.show_path && (cli.message.is_some() || !matches!(invocation, Invocation::Plan { .. })) {
        return Err(usage_err(
            "--show-path only works with a selector, e.g. `plan --show-path yesterday`.",
        ));
    }
    if cli.within.is_some() && !matches!(invocation, Invocation::Search(_)) {
        return Err(usage_err(
            "--in only works with `plan search`, e.g. `plan search deploy --in @~6..`.",
        ));
    }

    // Single scan for all commands: warns once, reused for selectors and search
    let mut plan_entries = Vec::new();
    if cfg.dir.exists() {
        let scan = file::scan_plan_dir(&cfg.dir, &cfg.scan.ignored_patterns)?;
        maybe_warn_unexpected(&cfg, &scan.unexpected);
        plan_entries = scan.plan_entries;
    }
    let today = date::now().date();
    let dates = plan_dates(&plan_entries);

    match &invocation {
        Invocation::Init(_) => unreachable!("handled above"),
        Invocation::Search(query) => {
            let mut paths: Vec<PathBuf> = match &cli.within {
                None => plan_entries.iter().map(|e| e.path()).collect(),
                Some(within) => {
                    let selector = select::parse(within).map_err(selector_err)?;
                    let days =
                        match select::resolve(&selector, today, &dates).map_err(selector_err)? {
                            Resolved::Day(d) => vec![d],
                            Resolved::Plans(v) => v,
                        };
                    days.into_iter()
                        .map(|d| date::get_plan_path(&cfg.dir, d))
                        .filter(|p| p.exists())
                        .collect()
                }
            };
            paths.sort();
            paths.reverse();

            let q_lower = query.to_lowercase();
            for path in paths {
                let filename = path.file_name().unwrap_or_default().to_string_lossy();
                if let Ok(content) = fs::read_to_string(&path) {
                    for (i, line) in content.lines().enumerate() {
                        if line.to_lowercase().contains(&q_lower) {
                            writeln!(io::stdout(), "{}:{}: {}", filename, i + 1, line)
                                .map_err(stdout_err)?;
                        }
                    }
                }
            }
        }
        Invocation::Plan { selector } => {
            let text = selector.as_deref().unwrap_or("@");
            let parsed = select::parse(text).map_err(|e| {
                // A lone word that isn't a selector was most likely a message.
                if !cli.show_path && cli.message.is_none() && looks_like_words(text) {
                    usage_err(format!(
                        "{e} To log a message, use -m: plan -m {:?}",
                        cli.args.join(" ")
                    ))
                } else {
                    selector_err(e)
                }
            })?;
            match select::resolve(&parsed, today, &dates).map_err(selector_err)? {
                Resolved::Day(day) => act_on(&cli, &cfg.dir, day, today)?,
                Resolved::Plans(plans) => {
                    let acting = cli.message.is_some() || cli.show_path;
                    let terminal = stdout_is_terminal();
                    match plans.len() {
                        0 if acting || terminal => {
                            return Err(usage_err(format!("No plans match `{text}`.")));
                        }
                        0 => return Err(silent_exit(2)),
                        // In a terminal, a single match opens like a day would.
                        1 if acting || terminal => act_on(&cli, &cfg.dir, plans[0], today)?,
                        n if acting => {
                            let what = if cli.message.is_some() {
                                "-m"
                            } else {
                                "--show-path"
                            };
                            let shown = &plans[..n.min(LIST_LIMIT)];
                            let candidates = list::render(
                                &rows(&cfg.dir, shown),
                                n,
                                today,
                                &dates,
                                false,
                                |_| None,
                            );
                            return Err(usage_err(format!(
                                "`{text}` matches {n} plans, but {what} needs exactly one:\n{}",
                                candidates
                                    .lines()
                                    .map(|l| format!("  {l}"))
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            )));
                        }
                        n => {
                            // A terminal gets a page and a pointer to the rest;
                            // a pipe gets every match, so scripts see them all.
                            let (shown, color) = if terminal {
                                (
                                    &plans[..n.min(LIST_LIMIT)],
                                    env::var_os("NO_COLOR").is_none(),
                                )
                            } else {
                                (&plans[..], false)
                            };
                            let Selector::Set(set) = &parsed else {
                                unreachable!("only sets resolve to plans")
                            };
                            let out = list::render(
                                &rows(&cfg.dir, shown),
                                n,
                                today,
                                &dates,
                                color,
                                |oldest| select::earlier(set, today, &dates, oldest),
                            );
                            write!(io::stdout(), "{out}").map_err(stdout_err)?;
                        }
                    }
                }
            }
        }
    }

    io::stdout().flush().map_err(stdout_err)?;
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        if let Some(plan_err) = e.downcast_ref::<PlanError>() {
            match plan_err {
                PlanError::Usage(msg) => {
                    let _ = writeln!(io::stderr(), "plan: {}", msg);
                    std::process::exit(2);
                }
                PlanError::SilentExit(code) => {
                    std::process::exit(*code);
                }
            }
        } else {
            let _ = writeln!(io::stderr(), "Error: {:#}", e);
            std::process::exit(1);
        }
    }
}
