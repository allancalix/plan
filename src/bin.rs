use anyhow::{Context, Result, bail};
use plan::config;
use plan::date::{self, Selector};
use plan::file;

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

#[derive(Parser, Debug)]
#[command(
    version,
    about = "A standalone tool for writing and managing daily plan files.",
    long_about = None,
    override_usage = "plan [OPTIONS] [SELECTOR]\n       plan [OPTIONS] [SELECTOR] -m <MESSAGE>\n       plan [OPTIONS] ls\n       plan [OPTIONS] search <QUERY...>\n       plan init <DIR>",
    after_help = "Selectors: @ (today), @~N, @- (most recent plan), today, yesterday, \"N days ago\"\n\nCommands:\n  ls              List recent plan files with dates and line counts\n  search <QUERY>  Search all plan files (case-insensitive substring)\n  init <DIR>      Configure the plan directory, creating it if needed\n\nExamples:\n  plan                            open today's plan in $VISUAL/$EDITOR\n  plan yesterday | bat            print yesterday's plan (printing never creates one)\n  plan -m \"ship the thing\"        log an entry into today's inbox\n  plan yesterday -m \"forgot this\"  log into yesterday's plan\n  echo note | plan @- -m -        log a line from stdin into the most recent plan"
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

    /// A selector (today's plan if omitted), or a command: ls, search, init
    #[arg(value_name = "SELECTOR")]
    args: Vec<String>,
}

/// What the positional words ask for.
enum Invocation {
    Init(String),
    Ls,
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
        ("ls", []) => Some(Invocation::Ls),
        ("ls", _) => return Err(usage_err("usage: plan ls")),
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
    let is_selector = date::parse_selector(Some(first)).is_ok();
    // A mistyped `@...` selector is a typo, not a message.
    if !is_selector && first.trim_start().starts_with('@') && !rest.is_empty() {
        let e = date::parse_selector(Some(first)).unwrap_err();
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
        (_, true, None) => {
            return Ok(Invocation::Plan {
                selector: Some(first.to_string()),
            });
        }
        // `plan yesterday` is fine; `plan ship` is reported by `resolve`.
        (None, false, None) => {
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

/// Commands that were folded into the bare `plan` form, with what to type now.
fn removed_command_hint(word: &str) -> Option<&'static str> {
    match word {
        "log" | "jot" => {
            Some("`plan log` is now `plan [selector] -m <message>`, e.g. `plan -m \"ship it\"`")
        }
        "show" => Some(
            "`plan show` is now `plan <selector>` with stdout piped, e.g. `plan yesterday | bat`",
        ),
        _ => None,
    }
}

/// A resolved plan file: where it is, and what to do if it doesn't exist.
struct Target {
    path: PathBuf,
    /// The date, when the file may need creating; `None` for `@-`, which only
    /// ever names a file that exists.
    date: Option<jiff::civil::Date>,
    days_ago: u32,
}

/// `hint`, if given, is what to suggest when the selector doesn't parse.
fn resolve(
    selector: Option<&str>,
    hint: Option<&str>,
    dir: &Path,
    latest: Option<PathBuf>,
) -> Result<Target> {
    let parsed = date::parse_selector(selector).map_err(|e| match hint {
        Some(hint) => usage_err(format!("{e} {hint}")),
        None => usage_err(e.to_string()),
    })?;
    match parsed {
        Selector::Latest => match latest {
            Some(path) => Ok(Target {
                path,
                date: None,
                days_ago: 0,
            }),
            None => bail!("No plan files found in {}", dir.display()),
        },
        Selector::DaysAgo(days_ago) => {
            let naive = date::get_date(days_ago).map_err(|e| usage_err(e.to_string()))?;
            Ok(Target {
                path: date::get_plan_path(dir, naive),
                date: Some(naive),
                days_ago,
            })
        }
    }
}

/// Create today's plan from the template if it's missing. Past dates must
/// already exist: plan never invents history.
fn ensure_exists(target: &Target, guard: &file::LockGuard) -> Result<()> {
    let Some(naive) = target.date else {
        return Ok(());
    };
    let path = &target.path;
    if let Err(e) = date::ensure_file_exists(path, naive, target.days_ago > 0, guard) {
        if e.kind() == io::ErrorKind::NotFound {
            return Err(usage_err(format!(
                "No plan file for that date: {}",
                path.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string())
            )));
        } else {
            return Err(e).context("Error ensuring file exists");
        }
    }
    Ok(())
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

fn run() -> Result<()> {
    let cli = Cli::parse();
    let invocation = parse_invocation(&cli.args, cli.message.as_deref())?;

    if let Invocation::Init(dir) = &invocation {
        return init(dir);
    }

    let mut cfg = config::Config::load()?;

    if let Some(dir) = cli.dir {
        cfg.dir = config::expand_tilde(&dir);
        if !cfg.dir.exists() {
            fs::create_dir_all(&cfg.dir)
                .context(format!("Error creating directory {}", cfg.dir.display()))?;
        }
    }

    let selecting_only = matches!(invocation, Invocation::Plan { .. }) && cli.message.is_none();
    if cli.show_path && !selecting_only {
        return Err(usage_err(
            "--show-path only works with a selector, e.g. `plan --show-path yesterday`.",
        ));
    }

    // Single scan for all commands: warns once, reused by ls/search/@-
    let mut plan_entries = Vec::new();
    if cfg.dir.exists() {
        let scan = file::scan_plan_dir(&cfg.dir, &cfg.scan.ignored_patterns)?;
        maybe_warn_unexpected(&cfg, &scan.unexpected);
        plan_entries = scan.plan_entries;
    }

    match &invocation {
        Invocation::Init(_) => unreachable!("handled above"),
        Invocation::Ls => {
            plan_entries.sort_by_key(|e| e.file_name());
            plan_entries.reverse();

            for entry in plan_entries.iter().take(30) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let date_str = &name[..name.len() - 5];
                if let Ok(parsed) = jiff::civil::Date::strptime("%Y-%m-%d", date_str) {
                    let day_of_week = parsed.strftime("%a").to_string();
                    let content = fs::read_to_string(&path)?;
                    let lines = content.lines().count();
                    writeln!(
                        io::stdout(),
                        "{}  {}  {:>2} lines",
                        date_str,
                        day_of_week,
                        lines
                    )
                    .map_err(stdout_err)?;
                }
            }
        }
        Invocation::Search(query) => {
            let q_lower = query.to_lowercase();
            plan_entries.sort_by_key(|e| e.file_name());
            plan_entries.reverse();

            for entry in plan_entries {
                let path = entry.path();
                let filename = entry.file_name().to_string_lossy().into_owned();
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
            let latest = file::find_latest(&plan_entries);
            // Words that don't start with a selector were most likely meant as a
            // message; a malformed `@...` selector is just a typo.
            let hint = (!cli.show_path
                && selector
                    .as_deref()
                    .is_some_and(|s| !s.trim_start().starts_with('@')))
            .then(|| format!("To log a message, use -m: plan -m {:?}", cli.args.join(" ")));
            let target = resolve(selector.as_deref(), hint.as_deref(), &cfg.dir, latest)?;

            if let Some(message) = &cli.message {
                // `plan [selector] -m <message>`: log into the inbox.
                let text = if message == "-" {
                    read_stdin_line()?
                } else {
                    message.trim().to_string()
                };
                if text.is_empty() {
                    return Err(usage_err("Message cannot be empty."));
                }
                let lock = file::acquire_lock(&target.path)?;
                ensure_exists(&target, &lock)?;
                // Stamp the entry under the lock, so entries committed one after
                // another are never stamped out of order.
                let entry = format!("{} {}", date::entry_time(), text);
                file::insert_into_inbox(&target.path, &entry, &lock)?;
            } else if cli.show_path || stdout_is_terminal() {
                {
                    let lock = file::acquire_lock(&target.path)?;
                    ensure_exists(&target, &lock)?;
                }
                if cli.show_path {
                    writeln!(io::stdout(), "{}", target.path.display()).map_err(stdout_err)?;
                } else {
                    open_editor(&target.path)?;
                }
            } else {
                // Piped (`plan yesterday | bat`): print the plan. Never creates one.
                if !target.path.exists() {
                    return Err(silent_exit(2));
                }
                let content = {
                    let _lock = file::acquire_shared_lock(&target.path)?;
                    fs::read_to_string(&target.path)?
                };
                // Print only after releasing the lock: stdout may be a pipe nobody
                // is reading (a pager left open), and every writer would wait on it.
                write!(io::stdout(), "{}", content).map_err(stdout_err)?;
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
