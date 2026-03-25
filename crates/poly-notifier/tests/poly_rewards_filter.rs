use std::process::Command;

#[test]
fn poly_rewards_filter_prints_help() {
    let output = Command::new(env!("CARGO_BIN_EXE_poly-rewards-filter"))
        .arg("--help")
        .output()
        .expect("run poly-rewards-filter --help");

    assert!(output.status.success());
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("Usage: poly-rewards-filter"));
    assert!(stdout.contains("--report"));
    assert!(stdout.contains("--sort"));
}

#[test]
fn poly_rewards_filter_rejects_unknown_sort_key() {
    let output = Command::new(env!("CARGO_BIN_EXE_poly-rewards-filter"))
        .args([
            "--report",
            "outputs/rewards/report.json",
            "--sort",
            "unknown",
        ])
        .output()
        .expect("run poly-rewards-filter with bad sort");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("utf8 stderr");
    assert!(stderr.contains("invalid sort key"));
}
