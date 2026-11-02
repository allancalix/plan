//! Housekeeping around atomic writes: temp files never cause warnings, are
//! swept up after interrupted writes, and locking leaves nothing behind.

use plan::file;
use std::fs;
use tempfile::TempDir;

const TEMPLATE: &str =
    "2026, Feb 19 - Thursday\n~~~~~~~~~inbox~~~~~~~~~\n~~~~~~~~~~~~~~~~~~~~~~~\n\n---\n";

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn temp_files_are_not_unexpected() {
    let temp = TempDir::new().unwrap();
    for name in [
        "2026-02-19.plan",
        "2026-02-19.4242.tmp", // in flight, or left by a killed writer
        "2026-02-19.tmp-4242", // same, from older versions of plan
        "notes.txt",
    ] {
        fs::write(temp.path().join(name), "").unwrap();
    }
    let scan = file::scan_plan_dir(temp.path(), &[]).unwrap();
    assert_eq!(scan.unexpected, ["notes.txt"]);
}

#[test]
fn writes_sweep_stale_temp_files_for_the_same_plan() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("2026-02-19.plan");
    fs::write(&path, TEMPLATE).unwrap();
    for name in [
        "2026-02-19.111.tmp",
        "2026-02-19.tmp-222",
        "2026-02-20.333.tmp", // another plan's temp file: not ours to touch
        "2026-02-19.notes.tmp",
    ] {
        fs::write(temp.path().join(name), "stale").unwrap();
    }

    let lock = file::acquire_lock(&path).unwrap();
    file::insert_into_inbox(&path, "* hello", &lock).unwrap();
    drop(lock);

    assert!(fs::read_to_string(&path).unwrap().contains("* hello"));
    assert_eq!(
        names(temp.path()),
        [
            "2026-02-19.notes.tmp",
            "2026-02-19.plan",
            "2026-02-20.333.tmp"
        ]
    );
}

#[test]
fn locking_leaves_no_files_behind() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("2026-02-19.plan");
    fs::write(&path, TEMPLATE).unwrap();
    {
        let lock = file::acquire_lock(&path).unwrap();
        file::insert_into_inbox(&path, "* hello", &lock).unwrap();
    }
    drop(file::acquire_shared_lock(&path).unwrap());
    assert_eq!(names(temp.path()), ["2026-02-19.plan"]);
}
