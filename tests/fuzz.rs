#![cfg(feature = "test-clock")]

use assert_cmd::Command;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};
use std::collections::HashSet;
use std::fs;
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

#[test]
fn test_concurrent_fuzzer() {
    let temp = TempDir::new().expect("Failed to create temp dir");
    let plan_dir = temp.path().join("plan_files");
    fs::create_dir_all(&plan_dir).unwrap();
    let mock_date = "2026-02-19";

    let num_threads = 20;
    let ops_per_thread = 20;

    let seed_env = std::env::var("FUZZ_SEED").unwrap_or_else(|_| "".to_string());
    let seed: u64 = if seed_env.is_empty() {
        rand::random::<u64>()
    } else {
        seed_env.parse().expect("FUZZ_SEED must be an integer u64")
    };
    println!("=== FUZZER SEED: {} ===", seed);

    let mut handles = vec![];

    for t_idx in 0..num_threads {
        let plan_dir = plan_dir.clone();
        let thread_seed = seed.wrapping_add(t_idx as u64);

        let handle = thread::spawn(move || {
            let mut expected_inserts = Vec::new();
            let mut rng = StdRng::seed_from_u64(thread_seed);

            for op_idx in 0..ops_per_thread {
                let is_task = rng.random_bool(0.7); // 70% tasks, 30% notes using 0.10.0 syntax
                let text = format!("thread_{}_op_{}", t_idx, op_idx);

                // In order to use PLAN_MOCK_TIME, wait for assert_cmd to automatically select the local test-compiled binary
                let plan_bin = assert_cmd::cargo::cargo_bin!("plan");
                let mut command = Command::new(plan_bin);
                command
                    .env("PLAN_DIR", &plan_dir)
                    .env("PLAN_MOCK_TIME", mock_date);

                if is_task {
                    command.arg("log").arg(&text);
                    expected_inserts.push(format!("* {}", text));
                } else {
                    command.arg("jot").arg(&text);
                    expected_inserts.push(text.clone());
                }

                // Random latency to maximize scheduling collisions across OS
                thread::sleep(Duration::from_millis(rng.random_range(0..10)));

                command.assert().success();

                // Fuzz timing slightly again post-write
                thread::sleep(Duration::from_millis(rng.random_range(0..5)));
            }
            expected_inserts
        });
        handles.push(handle);
    }

    let mut all_expected = HashSet::new();
    for handle in handles {
        let expected = handle.join().expect("Thread panicked (deadlock or crash?)");
        for e in expected {
            all_expected.insert(e);
        }
    }

    let file_path = plan_dir.join(format!("{}.plan", mock_date));
    assert!(file_path.exists(), "Plan file was not created");

    let content = fs::read_to_string(&file_path).unwrap();

    // Verify every expected line is present EXACTLY once
    let mut actual_lines = HashSet::new();
    for line in content.lines() {
        if line.starts_with("thread_") || line.starts_with("* thread_") {
            // Handle duplicates if any, since our inputs are globally unique per op
            let is_new = actual_lines.insert(line.to_string());
            assert!(
                is_new,
                "Duplicate line found indicating corrupted write boundary: {}",
                line
            );
        }
    }

    // Verify no dropped text
    for expected in &all_expected {
        assert!(
            actual_lines.contains(expected),
            "Dropped data! Missing: {}",
            expected
        );
    }

    // Assert counts strictly
    assert_eq!(
        all_expected.len(),
        actual_lines.len(),
        "Mismatch in total operations and recorded lines"
    );
}
