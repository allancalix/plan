#![cfg(feature = "test-clock")]
//! Runs the Neovim plugin's end-to-end tests (`nvim/test/plugin_test.lua`)
//! against the freshly built binary, so a CLI change that breaks the plugin
//! fails `cargo test`. Skipped, loudly, when `nvim` isn't installed.

use std::process::Command;

#[test]
fn nvim_plugin() {
    let Ok(version) = Command::new("nvim").arg("--version").output() else {
        eprintln!("skipping nvim plugin tests: nvim is not installed");
        return;
    };
    let version = String::from_utf8_lossy(&version.stdout);
    let output = Command::new("nvim")
        .args(["--headless", "--clean", "-l", "nvim/test/plugin_test.lua"])
        .env("PLAN_BIN", env!("CARGO_BIN_EXE_plan"))
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .expect("run nvim");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "nvim plugin tests failed ({})\n{stdout}\n{stderr}",
        version.lines().next().unwrap_or("unknown nvim")
    );
    println!("{stdout}");
}
