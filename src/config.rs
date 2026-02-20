use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

pub struct ScanConfig {
    pub warn_unexpected: bool,
    pub ignored_patterns: Vec<String>,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            warn_unexpected: true,
            ignored_patterns: Vec::new(),
        }
    }
}

pub struct Config {
    pub dir: PathBuf,
    pub scan: ScanConfig,
}

/// Strip surrounding quotes from a value (handles both `"val"` and `'val'`).
fn strip_quotes(s: &str) -> &str {
    if s.len() >= 2
        && ((s.starts_with('"') && s.ends_with('"'))
            || (s.starts_with('\'') && s.ends_with('\'')))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Parse all `key = value` pairs from INI-style content.
fn parse_ini(content: &str) -> Vec<(&str, &str)> {
    content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                return None;
            }
            let (key, val) = line.split_once('=')?;
            Some((key.trim(), strip_quotes(val.trim())))
        })
        .collect()
}

fn scan_config_from_pairs(pairs: &[(&str, &str)]) -> ScanConfig {
    let warn = pairs
        .iter()
        .find(|(k, _)| *k == "warn_unexpected")
        .map(|(_, v)| *v != "false")
        .unwrap_or(true);
    let ignored: Vec<String> = pairs
        .iter()
        .filter(|(k, _)| *k == "ignore")
        .map(|(_, v)| v.to_string())
        .collect();
    ScanConfig {
        warn_unexpected: warn,
        ignored_patterns: ignored,
    }
}

fn config_from_pairs(pairs: &[(&str, &str)]) -> Option<Config> {
    let dir = pairs.iter().find(|(k, _)| *k == "dir")?.1;
    Some(Config {
        dir: expand_tilde(dir),
        scan: scan_config_from_pairs(pairs),
    })
}

impl Config {
    pub fn load() -> io::Result<Self> {
        // Load config file content (if it exists) for scan settings
        let config_path = get_config_path();
        let config_content = fs::read_to_string(&config_path).ok();
        let pairs: Vec<(&str, &str)> = config_content.as_deref().map(parse_ini).unwrap_or_default();

        // 1. Env var overrides directory
        if let Ok(dir) = env::var("PLAN_DIR")
            && !dir.is_empty()
        {
            return Ok(Self {
                dir: expand_tilde(&dir),
                scan: scan_config_from_pairs(&pairs),
            });
        }

        // 2. Config file
        if let Some(cfg) = config_from_pairs(&pairs) {
            return Ok(cfg);
        }

        // 3. Prompt on first run
        println!("No plan directory configured.");
        print!("Enter path [~/plan]: ");
        io::stdout().flush()?;

        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let input = input.trim();

        let dir_str = if input.is_empty() { "~/plan" } else { input };
        let dir_path = expand_tilde(dir_str);

        // write config file
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, format!("dir = {dir_str}\n"))?;

        Ok(Self {
            dir: dir_path,
            scan: ScanConfig::default(),
        })
    }

    pub fn init(dir_str: &str) -> io::Result<Self> {
        let config_path = get_config_path();
        let dir_path = expand_tilde(dir_str);

        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&config_path, format!("dir = {dir_str}\n"))?;
        Ok(Self {
            dir: dir_path,
            scan: ScanConfig::default(),
        })
    }
}

pub fn get_config_path() -> PathBuf {
    if let Ok(xdg) = env::var("XDG_CONFIG_HOME")
        && !xdg.is_empty()
    {
        return PathBuf::from(xdg).join("plan").join("config");
    }

    // fallback to ~/.config/plan/config
    let mut path = expand_tilde("~/.config");
    path.push("plan");
    path.push("config");
    path
}

pub fn expand_tilde(path: &str) -> PathBuf {
    if (path.starts_with("~/") || path == "~")
        && let Ok(home) = env::var("HOME")
    {
        let mut buf = PathBuf::from(home);
        if path.len() > 2 {
            buf.push(&path[2..]);
        }
        return buf;
    }
    PathBuf::from(path)
}
