#![cfg(feature = "test-clock")]

use assert_cmd::Command;
use std::fs;
use std::thread;
use tempfile::TempDir;

#[test]
fn test_concurrent_inserts() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let plan_dir = temp.path().join("plan_files");
    fs::create_dir_all(&plan_dir).unwrap();
    let mock_date = "2026-02-19";

    // Initialize an empty file or just let the first command create it
    let mut handles = vec![];
    let num_threads = 20;

    for i in 0..num_threads {
        let plan_dir = plan_dir.clone();
        let handle = thread::spawn(move || {
            let plan_bin = assert_cmd::cargo::cargo_bin!("plan");
            let mut command = Command::new(plan_bin);
            command
                .env("PLAN_DIR", &plan_dir)
                .env("PLAN_MOCK_TIME", mock_date)
                .arg("log")
                .arg(format!("task from thread {}", i));

            command.assert().success();
        });
        handles.push(handle);
    }

    for handle in handles {
        handle.join().unwrap();
    }

    let file_path = plan_dir.join(format!("{}.plan", mock_date));
    assert!(file_path.exists(), "Plan file was not created");

    let content = fs::read_to_string(&file_path).unwrap();
    let tasks_found = content
        .lines()
        .filter(|l| l.starts_with("* task from thread "))
        .count();

    assert_eq!(
        tasks_found, num_threads,
        "Concurrent writes dropped data! Expected {} tasks, found {}.\nFile content:\n{}",
        num_threads, tasks_found, content
    );
}
