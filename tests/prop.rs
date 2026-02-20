use plan::file;
use proptest::prelude::*;
use std::fs;
use tempfile::TempDir;

proptest! {
    #![proptest_config(ProptestConfig {
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    // 1. Fuzz the bounds of date parsing to ensure no `unwrap()` panics on subtraction
    #[test]
    fn test_date_calculation_fuzz(days_ago in 0u32..u32::MAX) {
        // Assert that computing extremely huge bounds natively via our library API never triggers a rust panic,
        // but securely returns None so the bin wrapper can handle process exiting
        let _ = plan::date::get_date_opt(days_ago);
    }

    // 2. Fuzz the `insert_into_inbox` parser with wildly chaotic file bodies
    // to ensure it never deletes data or panics due to malformed `[:inbox` states
    #[test]
    fn test_inbox_insertion_fuzz(
        mock_body in ".*",
        mock_task in "[^\n]+" // Any string without newlines
    ) {
        let temp = TempDir::new().unwrap();
        let plan_dir = temp.path().join("fuzz_dir");
        fs::create_dir_all(&plan_dir).unwrap();
        let file_path = plan_dir.join("fuzz.plan");

        // Write the fuzzed garbage state
        fs::write(&file_path, &mock_body).unwrap();

        // Attempt to insert
        let lock = file::acquire_lock(&file_path).unwrap();
        let res = file::insert_into_inbox(&file_path, &format!("* {}", mock_task), &lock);

        // The result should either succeed natively or return an OS Error if the fs hits weird limits.
        // But what it absolutely MUST NOT do is panic (unless missing box bounds and exiting cleanly,
        // though we fixed that by auto-generating bounds now so it should always succeed!)

        if res.is_ok() {
            let new_content = fs::read_to_string(&file_path).unwrap();

            // Fuzzer Guarantee 1: The new task MUST exist in the file
            assert!(new_content.contains(&mock_task), "Dropped task during fuzz insertion!");

            // Fuzzer Guarantee 2: The file MUST still end with a closing tilde line
            let last_line = new_content.trim_end().lines().last().unwrap_or("");
            assert!(!last_line.is_empty() && last_line.chars().all(|c| c == '~'),
                "File did not properly close the inbox block: {}", new_content);
        }
    }

    #[test]
    fn test_parse_date_opt_valid(days in 0..10_000u32) {
        let n_days = format!("{} days ago", days);
        assert_eq!(plan::date::parse_date_opt(Some(&n_days)).unwrap(), days);

        let n_day = format!("{} day ago", days);
        assert_eq!(plan::date::parse_date_opt(Some(&n_day)).unwrap(), days);

        let tilde = format!("@~{}", days);
        assert_eq!(plan::date::parse_date_opt(Some(&tilde)).unwrap(), days);
    }

    #[test]
    fn test_parse_date_opt_garbage(ref s in ".*") {
        let res = plan::date::parse_date_opt(Some(s));
        let s_lower = s.trim().to_lowercase();

        if s_lower == "today" || s_lower == "@" {
            assert_eq!(res.unwrap(), 0);
        } else if s_lower == "yesterday" {
            assert_eq!(res.unwrap(), 1);
        } else if let Some(stripped) = s_lower.strip_prefix("@~") {
            if let Ok(n) = stripped.parse::<u32>() {
                assert_eq!(res.unwrap(), n);
            } else {
                assert!(res.is_err());
            }
        } else if s_lower.ends_with(" days ago") {
            let num = s_lower.replace(" days ago", "");
            if let Ok(n) = num.trim().parse::<u32>() {
                assert_eq!(res.unwrap(), n);
            } else {
                assert!(res.is_err());
            }
        } else if s_lower.ends_with(" day ago") {
            let num = s_lower.replace(" day ago", "");
            if let Ok(n) = num.trim().parse::<u32>() {
                assert_eq!(res.unwrap(), n);
            } else {
                assert!(res.is_err());
            }
        } else {
            assert!(res.is_err());
        }
    }
}
