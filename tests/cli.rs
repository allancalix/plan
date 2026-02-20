#![cfg(feature = "test-clock")]

mod txtar;

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

struct TxtarTest {
    commands: Vec<String>,
    files: Vec<(String, String)>,
}

fn parse_txtar(content: &str) -> TxtarTest {
    let archive = txtar::Archive::from(content);
    let commands: Vec<String> = archive
        .comment()
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.trim().to_string())
        .collect();
    let files: Vec<(String, String)> = archive
        .iter()
        .map(|f| (f.name.clone(), f.content.clone()))
        .collect();
    TxtarTest { commands, files }
}

/// Collect all non-lock files from a directory.
fn collect_dir_files(dir: &PathBuf) -> Vec<(String, String)> {
    let mut entries: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    entries.sort_by_key(|e| e.path());

    entries
        .into_iter()
        .filter_map(|entry| {
            let file_path = entry.path();
            if !file_path.is_file() {
                return None;
            }
            let filename = file_path.file_name()?.to_string_lossy().to_string();
            if filename.ends_with(".lock") {
                return None;
            }
            let content = fs::read_to_string(&file_path).unwrap();
            Some((filename, content))
        })
        .collect()
}

fn write_txtar_file(
    path: &PathBuf,
    commands: &[String],
    plan_dir: &PathBuf,
    output_dir: &PathBuf,
) {
    let mut builder = txtar::Builder::new();
    let comment = commands.join("\n") + "\n";
    builder.comment(comment);

    // Merge files from both directories, sorted by name
    let mut all_files = collect_dir_files(plan_dir);
    all_files.extend(collect_dir_files(output_dir));
    all_files.sort_by(|a, b| a.0.cmp(&b.0));

    for (name, content) in all_files {
        builder.file((name, content));
    }

    fs::write(path, builder.build().to_string()).expect("Failed to write golden file");
}

fn run_txtar_test(path: PathBuf) {
    let content = fs::read_to_string(&path).expect("Failed to read txtar file");
    let test = parse_txtar(&content);

    let temp = TempDir::new().expect("Failed to create temp dir");
    let plan_dir = temp.path().join("plan_files");
    let output_dir = temp.path().join("cmd_output");
    fs::create_dir_all(&plan_dir).unwrap();
    fs::create_dir_all(&output_dir).unwrap();
    let mut mock_date = chrono::NaiveDate::from_ymd_opt(2026, 2, 19).unwrap();

    // Execute commands
    let mut executed_cmd_index = 1;
    for cmd in test.commands.iter() {
        if cmd.starts_with("#") {
            continue;
        }
        let expects_error = cmd.starts_with("! ");
        let cmd_clean = if expects_error {
            &cmd[2..]
        } else {
            cmd.as_str()
        };

        if cmd_clean.starts_with(">> forward ") {
            let parts: Vec<&str> = cmd_clean.split_whitespace().collect();
            if parts.len() == 4 {
                let amount: i64 = parts[2].parse().expect("Invalid forward amount");
                let days = match parts[3] {
                    "day" | "days" => amount,
                    "week" | "weeks" => amount * 7,
                    unit => panic!("Invalid forward unit: {}", unit),
                };
                mock_date += chrono::Duration::days(days);
            }
        } else if cmd_clean.starts_with("env ")
            || cmd_clean.starts_with("plan ")
            || cmd_clean == "plan"
        {
            let args_full =
                shlex::split(cmd_clean).expect("Invalid shell quoting in txtar commands");
            let mut env_vars = Vec::new();
            let mut args_iter = args_full.into_iter().peekable();

            if let Some(first) = args_iter.peek()
                && first == "env"
            {
                args_iter.next(); // consume "env"
                while let Some(arg) = args_iter.peek() {
                    if arg.contains('=') {
                        let arg_val = args_iter.next().unwrap();
                        let (k, v) = arg_val.split_once('=').unwrap();
                        let v = v.replace("$PLAN_DIR", &plan_dir.to_string_lossy());
                        env_vars.push((k.to_string(), v));
                    } else {
                        break;
                    }
                }
            }

            let cmd_name = args_iter.next().unwrap_or_default();
            if cmd_name != "plan" {
                panic!(
                    "test command must be 'plan' or 'env ... plan', found: {}",
                    cmd_name
                );
            }

            let plan_bin = assert_cmd::cargo::cargo_bin!("plan");
            let mut command = Command::new(plan_bin);
            command
                .env("PLAN_DIR", &plan_dir)
                .env("PLAN_MOCK_TIME", mock_date.format("%Y-%m-%d").to_string());

            let mut has_visual = false;
            let mut has_editor = false;
            for (k, v) in env_vars {
                command.env(&k, &v);
                if k == "VISUAL" {
                    has_visual = true;
                }
                if k == "EDITOR" {
                    has_editor = true;
                }
            }

            if !has_visual {
                command.env("VISUAL", "cat");
            }
            if !has_editor {
                command.env("EDITOR", "cat");
            }

            for arg in args_iter {
                command.arg(arg);
            }

            let output = command.output().expect("Failed to execute command");

            if expects_error && output.status.success() {
                panic!("Command expected to fail but succeeded: {}", cmd);
            }
            if (!expects_error) && !output.status.success() {
                let stderr_str = String::from_utf8_lossy(&output.stderr);
                panic!(
                    "Command expected to succeed but failed: {}\nStderr: {}",
                    cmd, stderr_str
                );
            }

            // Write command outputs to separate output_dir (not plan_dir)
            if !output.status.success() {
                fs::write(
                    output_dir.join(format!("cmd_{}_exit.txt", executed_cmd_index)),
                    format!("{}\n", output.status.code().unwrap_or(1)),
                )
                .unwrap();
            }
            if !output.stdout.is_empty() {
                fs::write(
                    output_dir.join(format!("cmd_{}_stdout.txt", executed_cmd_index)),
                    &output.stdout,
                )
                .unwrap();
            }
            if !output.stderr.is_empty() {
                let stderr_str = String::from_utf8_lossy(&output.stderr);
                let sanitized_stderr =
                    stderr_str.replace(&plan_dir.to_string_lossy().to_string(), "$PLAN_DIR");
                fs::write(
                    output_dir.join(format!("cmd_{}_stderr.txt", executed_cmd_index)),
                    sanitized_stderr.as_bytes(),
                )
                .unwrap();
            }
            executed_cmd_index += 1;
        } else if let Some(stripped) = cmd_clean.strip_prefix("echo ") {
            let is_append = cmd_clean.contains(">>");
            let parts: Vec<&str> = if is_append {
                stripped.splitn(2, ">>").collect()
            } else {
                stripped.splitn(2, ">").collect()
            };

            if parts.len() == 2 {
                let mut content = parts[0].trim();
                if (content.starts_with('"') && content.ends_with('"'))
                    || (content.starts_with('\'') && content.ends_with('\''))
                {
                    content = &content[1..content.len() - 1];
                }
                let file_path = plan_dir.join(parts[1].trim());

                if is_append {
                    let mut file = fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(file_path)
                        .unwrap();
                    use std::io::Write;
                    writeln!(file, "{}", content).unwrap();
                } else {
                    fs::write(file_path, format!("{}\n", content)).unwrap();
                }
            } else {
                panic!("echo command must use > or >> redirect in txtar: {}", cmd);
            }
        } else if let Some(stripped) = cmd_clean.strip_prefix("replace ") {
            let args = shlex::split(stripped).expect("Invalid syntax for replace command");
            if args.len() == 3 {
                let old_str = &args[0];
                let new_str = &args[1];
                let file_path = plan_dir.join(&args[2]);
                if file_path.exists() {
                    let content = fs::read_to_string(&file_path).unwrap();
                    let new_content = content.replace(old_str, new_str);
                    fs::write(file_path, new_content).unwrap();
                }
            } else {
                panic!(
                    "replace command requires exactly 3 quoted args: replace \"old\" \"new\" filename: {}",
                    cmd
                );
            }
        } else if let Some(stripped) = cmd_clean.strip_prefix("rm ") {
            let file_path = plan_dir.join(stripped.trim());
            if file_path.exists() {
                fs::remove_file(file_path).unwrap();
            }
        } else if let Some(stripped) = cmd_clean.strip_prefix("mkdir ") {
            let dir_path = plan_dir.join(stripped.trim());
            fs::create_dir_all(dir_path).unwrap();
        } else {
            panic!("Unsupported txtar command natively: {}", cmd);
        }
    }

    if env::var("UPDATE_GOLDEN").is_ok() {
        write_txtar_file(&path, &test.commands, &plan_dir, &output_dir);
        return;
    }

    // Collect files from both plan_dir and output_dir for comparison
    let mut disk_files = HashSet::new();
    for dir in [&plan_dir, &output_dir] {
        for entry in fs::read_dir(dir).unwrap().filter_map(Result::ok) {
            if entry.path().is_file() {
                let name = entry.file_name().to_string_lossy().to_string();
                if !name.ends_with(".lock") {
                    disk_files.insert(name);
                }
            }
        }
    }

    for (filename, expected_content) in &test.files {
        assert!(
            disk_files.contains(filename),
            "Snapshot file expected but not generated: {}\nExpected Content:\n{}",
            filename,
            expected_content
        );
        // Check both directories for the file
        let file_path = if plan_dir.join(filename).exists() {
            plan_dir.join(filename)
        } else {
            output_dir.join(filename)
        };
        let actual_content = fs::read_to_string(file_path).unwrap();
        assert_eq!(
            actual_content.trim_end(),
            expected_content.trim_end(),
            "Snapshot file mismatch for '{}'",
            filename
        );
        disk_files.remove(filename);
    }

    if !disk_files.is_empty() {
        panic!(
            "Unexpected files generated during test execution that are absent in files: block!\n{:?}",
            disk_files
        );
    }
}

macro_rules! txtar_test {
    ($name:ident, $path:expr) => {
        #[test]
        fn $name() {
            run_txtar_test(PathBuf::from($path));
        }
    };
}

txtar_test!(test_basic_workflow, "tests/data/basic_workflow.txtar");
txtar_test!(test_edge_cases, "tests/data/edge_cases.txtar");
txtar_test!(test_out_of_range_date, "tests/data/out_of_range_date.txtar");
txtar_test!(test_time_forwarding, "tests/data/time_forwarding.txtar");
txtar_test!(
    test_ambiguous_date_args,
    "tests/data/ambiguous_date_args.txtar"
);
txtar_test!(
    test_mutually_exclusive_args,
    "tests/data/mutually_exclusive_args.txtar"
);
txtar_test!(
    test_inbox_regeneration,
    "tests/data/inbox_regeneration.txtar"
);
txtar_test!(test_editor_parsing, "tests/data/editor_parsing.txtar");
txtar_test!(
    test_natural_language_dates,
    "tests/data/natural_language_dates.txtar"
);
txtar_test!(test_last_session, "tests/data/last_session.txtar");
txtar_test!(test_cli_contract, "tests/data/cli_contract.txtar");
txtar_test!(
    test_unexpected_files_warn,
    "tests/data/unexpected_files_warn.txtar"
);
txtar_test!(
    test_ignored_files_no_warn,
    "tests/data/ignored_files_no_warn.txtar"
);
txtar_test!(
    test_warn_disabled_config,
    "tests/data/warn_disabled_config.txtar"
);
txtar_test!(
    test_ignore_config,
    "tests/data/ignore_config.txtar"
);
