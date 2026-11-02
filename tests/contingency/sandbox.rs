//! An isolated filesystem for one run: its own HOME, XDG config dir, and plan
//! directory. Every `plan` process runs with a scrubbed environment pointing
//! here, so a run can never read or touch the user's real plans or config.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

pub struct Sandbox {
    pub root: PathBuf,
    pub home: PathBuf,
    pub config: PathBuf,
    pub plans: PathBuf,
}

/// The plan directory after the run, once every process is dead.
pub struct Snapshot {
    /// Plan file key (`YYYY-MM-DD`) -> raw content.
    pub files: BTreeMap<String, String>,
    /// Temp files from writes that were killed before their rename. plan
    /// ignores these and sweeps them up on the next write to the same file.
    pub temp: Vec<String>,
    /// Anything else.
    pub stray: Vec<String>,
}

impl Sandbox {
    pub fn create(root: &Path) -> io::Result<Self> {
        let sandbox = Self {
            root: root.to_path_buf(),
            home: root.join("home"),
            config: root.join("config"),
            plans: root.join("plans"),
        };
        for dir in [&sandbox.home, &sandbox.config, &sandbox.plans] {
            fs::create_dir_all(dir)?;
        }
        Ok(sandbox)
    }

    pub fn open(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            home: root.join("home"),
            config: root.join("config"),
            plans: root.join("plans"),
        }
    }

    pub fn command(&self, plan_bin: &Path, mock_date: &str) -> Command {
        let mut cmd = Command::new(plan_bin);
        cmd.env_clear()
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.config)
            .env("PLAN_DIR", &self.plans)
            .env("PLAN_MOCK_TIME", mock_date)
            // The harness never opens an editor; make sure one can't hang a run.
            .env("VISUAL", "false")
            .env("EDITOR", "false")
            .current_dir(&self.root);
        cmd
    }

    pub fn snapshot(&self) -> io::Result<Snapshot> {
        let mut files = BTreeMap::new();
        let mut temp = Vec::new();
        let mut stray = Vec::new();
        for entry in fs::read_dir(&self.plans)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if let Some(key) = name.strip_suffix(".plan") {
                files.insert(key.to_string(), fs::read_to_string(entry.path())?);
            } else if crate::model::is_temp_file(&name) {
                temp.push(name);
            } else {
                stray.push(name);
            }
        }
        temp.sort();
        stray.sort();
        Ok(Snapshot { files, temp, stray })
    }
}
