use anyhow::{Context, Result, bail};
use plan::config;
use plan::date;
use plan::file;

use clap::{Parser, Subcommand};
use std::env;
use std::fs;
use std::io;
use std::path::Path;
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

#[derive(Parser, Debug)]
#[command(version, about = "A standalone tool for writing and managing daily plan files.", long_about = None)]
struct Cli {
    /// Initialize a new plan directory and save to config
    #[arg(long)]
    init: bool,

    /// Override config with a new directory
    #[arg(long, value_name = "DIR")]
    dir: Option<String>,

    /// Relative date: @~N, today, yesterday, "N days ago"
    #[arg(name = "DATE")]
    date: Option<String>,

    /// Print the resolved file path to stdout (creates template if needed)
    #[arg(long)]
    path: bool,

    /// Open the most recent plan file chronologically
    #[arg(long, global = true)]
    last: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Insert '* <text>' into today's inbox (reads stdin if '-')
    Log {
        text: String,
        /// Relative date: @~N, today, yesterday, "N days ago"
        #[arg(name = "DATE")]
        date: Option<String>,
    },
    /// Insert raw note into today's inbox (reads stdin if '-')
    Jot {
        text: String,
        /// Relative date: @~N, today, yesterday, "N days ago"
        #[arg(name = "DATE")]
        date: Option<String>,
    },
    /// List recent plan files with dates and line counts
    Ls,
    /// Print a plan file to stdout (exit code 2 if not found)
    Show {
        /// Relative date: @~N, today, yesterday, "N days ago"
        #[arg(name = "DATE")]
        date: Option<String>,
    },
    /// Search across all plan files (substring match, case-insensitive)
    Search {
        /// The search query
        query: String,
    },
}

fn read_stdin_line() -> io::Result<String> {
    use std::io::BufRead;
    let mut line = String::new();
    io::stdin().lock().read_line(&mut line)?;
    Ok(line.trim().to_string())
}

fn open_editor(path: &std::path::Path) -> Result<()> {
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

fn maybe_warn_unexpected(cfg: &config::Config, unexpected: &[String]) {
    if cfg.scan.warn_unexpected {
        file::warn_unexpected_files(unexpected);
    }
}

fn parse_date_arg_or_error(arg: Option<&str>) -> Result<u32> {
    date::parse_date_opt(arg).map_err(|e| usage_err(e.to_string()))
}

fn handle_file_exists(path: &Path, naive_date: chrono::NaiveDate, days_ago: u32) -> Result<()> {
    if let Err(e) = date::ensure_file_exists(path, naive_date, days_ago > 0) {
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

fn run() -> Result<()> {
    let cli = Cli::parse();

    if cli.init {
        if let Some(dir) = cli.dir {
            let expanded_dir = config::expand_tilde(&dir);
            if !expanded_dir.exists() {
                fs::create_dir_all(&expanded_dir).context(format!(
                    "Error creating directory {}",
                    expanded_dir.display()
                ))?;
            }
            let _cfg = config::Config::init(&dir)?;
            println!("Configured plan directory: {}", dir);
            return Ok(());
        } else {
            return Err(usage_err("--init requires --dir=<path>"));
        }
    }

    let mut cfg = config::Config::load()?;

    if let Some(dir) = cli.dir {
        cfg.dir = config::expand_tilde(&dir);
        if !cfg.dir.exists() {
            fs::create_dir_all(&cfg.dir)
                .context(format!("Error creating directory {}", cfg.dir.display()))?;
        }
    }

    if cli.path && cli.command.is_some() {
        return Err(usage_err(
            "--path can only be used with the default command.",
        ));
    }

    // Single scan for all commands — warns once, reused by ls/search/--last
    let mut plan_entries = Vec::new();
    if cfg.dir.exists() {
        let scan = file::scan_plan_dir(&cfg.dir, &cfg.scan.ignored_patterns)?;
        maybe_warn_unexpected(&cfg, &scan.unexpected);
        plan_entries = scan.plan_entries;
    }

    let latest_plan = file::find_latest(&plan_entries);

    match &cli.command {
        Some(Commands::Log { text: val, date }) | Some(Commands::Jot { text: val, date }) => {
            let is_task = matches!(cli.command, Some(Commands::Log { .. }));
            let text = if val == "-" {
                read_stdin_line()?
            } else {
                val.trim().to_string()
            };
            if text.is_empty() {
                return Err(usage_err("Message cannot be empty."));
            }

            let actual_date = date.as_deref().or(cli.date.as_deref());
            if actual_date.is_some() && cli.last {
                return Err(usage_err("Cannot use --last with a specific date."));
            }

            let (path, target_date, days_ago) = if cli.last {
                if let Some(p) = latest_plan {
                    (p, None, None)
                } else {
                    bail!("No plan files found in {}", cfg.dir.display());
                }
            } else {
                let days = parse_date_arg_or_error(actual_date)?;
                let naive = date::get_date(days).map_err(|e| usage_err(e.to_string()))?;
                (
                    date::get_plan_path(&cfg.dir, naive),
                    Some(naive),
                    Some(days),
                )
            };

            let lock = file::acquire_lock(&path)?;

            if let (Some(naive), Some(days)) = (target_date, days_ago) {
                handle_file_exists(&path, naive, days)?;
            }

            let final_text = if is_task {
                format!("* {}", text)
            } else {
                text.to_string()
            };
            file::insert_into_inbox(&path, &final_text, &lock)?;
        }
        Some(Commands::Ls) => {
            if cli.last {
                return Err(usage_err("--last is not supported with the 'ls' command."));
            }

            plan_entries.sort_by_key(|e| e.file_name());
            plan_entries.reverse();

            for entry in plan_entries.iter().take(30) {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                let date_str = &name[..name.len() - 5];
                if let Ok(parsed) = chrono::NaiveDate::parse_from_str(date_str, "%Y-%m-%d") {
                    let day_of_week = parsed.format("%a").to_string();
                    let content = fs::read_to_string(&path)?;
                    let lines = content.lines().count();
                    println!("{}  {}  {:>2} lines", date_str, day_of_week, lines);
                }
            }
        }
        Some(Commands::Show { date }) => {
            let actual_date = date.as_deref().or(cli.date.as_deref());
            if actual_date.is_some() && cli.last {
                return Err(usage_err("Cannot use --last with a specific date."));
            }

            let path = if cli.last {
                if let Some(p) = latest_plan {
                    p
                } else {
                    bail!("No plan files found in {}", cfg.dir.display());
                }
            } else {
                let days_ago = parse_date_arg_or_error(actual_date)?;
                let naive_date = date::get_date(days_ago).map_err(|e| usage_err(e.to_string()))?;
                date::get_plan_path(&cfg.dir, naive_date)
            };

            if !path.exists() {
                return Err(silent_exit(2));
            }
            let _lock = file::acquire_shared_lock(&path)?;
            let content = fs::read_to_string(&path)?;
            print!("{}", content);
        }
        Some(Commands::Search { query }) => {
            if cli.last {
                return Err(usage_err(
                    "--last is not supported with the 'search' command.",
                ));
            }

            let q_lower = query.to_lowercase();
            plan_entries.sort_by_key(|e| e.file_name());
            plan_entries.reverse();

            for entry in plan_entries {
                let path = entry.path();
                let filename = entry.file_name().to_string_lossy().into_owned();
                if let Ok(content) = fs::read_to_string(&path) {
                    for (i, line) in content.lines().enumerate() {
                        if line.to_lowercase().contains(&q_lower) {
                            println!("{}:{}: {}", filename, i + 1, line);
                        }
                    }
                }
            }
        }
        None => {
            let actual_date = cli.date.as_deref();
            if actual_date.is_some() && cli.last {
                return Err(usage_err("Cannot use --last with a specific date."));
            }

            if cli.last {
                if let Some(path) = latest_plan {
                    if cli.path {
                        println!("{}", path.display());
                    } else {
                        open_editor(&path)?;
                    }
                } else {
                    bail!("No plan files found in {}", cfg.dir.display());
                }
            } else {
                let days_ago = parse_date_arg_or_error(actual_date)?;
                let naive_date = date::get_date(days_ago).map_err(|e| usage_err(e.to_string()))?;
                let path = date::get_plan_path(&cfg.dir, naive_date);
                {
                    let _lock = file::acquire_lock(&path)?;
                    handle_file_exists(&path, naive_date, days_ago)?;
                }
                if cli.path {
                    println!("{}", path.display());
                } else {
                    open_editor(&path)?;
                }
            }
        }
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        if let Some(plan_err) = e.downcast_ref::<PlanError>() {
            match plan_err {
                PlanError::Usage(msg) => {
                    eprintln!("plan: {}", msg);
                    std::process::exit(2);
                }
                PlanError::SilentExit(code) => {
                    std::process::exit(*code);
                }
            }
        } else {
            eprintln!("Error: {:#}", e);
            std::process::exit(1);
        }
    }
}
