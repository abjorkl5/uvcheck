use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

fn temporary_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_nanos();
    std::env::temp_dir().join(format!("uvcheck-rs-{}-{nonce}-{name}", std::process::id()))
}

fn run_uvcheck(state_file: &Path, config_file: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_uvcheck"))
        .args(arguments)
        .env("UVCHECK_STATE_FILE", state_file)
        .env("UVCHECK_CONFIG_FILE", config_file)
        .output()
        .expect("failed to run uvcheck")
}

#[test]
fn session_counts_commands_and_returns_failure() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");

    assert!(run_uvcheck(&state_file, &config_file, &["--start"])
        .status
        .success());
    assert!(
        run_uvcheck(&state_file, &config_file, &["python", "-c", "pass"])
            .status
            .success()
    );
    assert_eq!(
        run_uvcheck(
            &state_file,
            &config_file,
            &["python", "-c", "raise SystemExit(3)"],
        )
        .status
        .code(),
        Some(3),
    );

    let stopped = run_uvcheck(&state_file, &config_file, &["--stop"]);
    assert_eq!(stopped.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&stopped.stdout).contains("FAIL (1 PASS, 1 FAIL)"));
    assert!(!state_file.exists());
}

#[test]
fn no_arguments_runs_configured_commands() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    fs::write(
        &config_file,
        "#!/bin/bash\n\
         # Keep running after a failure\n\
         uv run python -c 'raise SystemExit(3)'\n\
         uv run python -c 'print(\"configured check\")'\n",
    )
    .expect("failed to write test configuration");

    let completed = run_uvcheck(&state_file, &config_file, &[]);
    let stdout = String::from_utf8_lossy(&completed.stdout);
    assert_eq!(completed.status.code(), Some(1));
    assert!(stdout.contains("configured check"));
    assert!(stdout.contains("FAIL (1 PASS, 1 FAIL)"));
    assert!(!state_file.exists());

    fs::remove_file(config_file).expect("failed to remove test configuration");
}

#[test]
fn no_arguments_runs_shorthand_configured_commands_through_uv_run() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    fs::write(
        &config_file,
        "python -c 'print(\"shorthand configured check\")'\n",
    )
    .expect("failed to write test configuration");

    let completed = run_uvcheck(&state_file, &config_file, &[]);
    let stdout = String::from_utf8_lossy(&completed.stdout);
    assert!(completed.status.success());
    assert!(stdout.contains("+ uv run python -c"));
    assert!(stdout.contains("shorthand configured check"));

    fs::remove_file(config_file).expect("failed to remove test configuration");
}

#[test]
fn json_command_report_captures_output_and_status() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");

    let completed = run_uvcheck(
        &state_file,
        &config_file,
        &[
            "--json",
            "python",
            "-c",
            "import sys; print('ok'); print('note', file=sys.stderr)",
        ],
    );
    let report: serde_json::Value = serde_json::from_slice(&completed.stdout).unwrap();

    assert!(completed.status.success());
    assert_eq!(report["status"], "PASS");
    assert_eq!(report["exit_code"], 0);
    assert_eq!(report["stdout"], "ok\n");
    assert_eq!(report["stderr"], "note\n");
}

#[test]
fn json_failed_command_preserves_exit_code() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");

    let completed = run_uvcheck(
        &state_file,
        &config_file,
        &["--json", "python", "-c", "raise SystemExit(3)"],
    );
    let report: serde_json::Value = serde_json::from_slice(&completed.stdout).unwrap();

    assert_eq!(completed.status.code(), Some(3));
    assert_eq!(report["status"], "FAIL");
    assert_eq!(report["exit_code"], 3);
}

#[test]
fn json_configured_report_includes_command_results_and_summary() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    fs::write(&config_file, "python -c 'print(\"configured\")'\n")
        .expect("failed to write test configuration");

    let completed = run_uvcheck(&state_file, &config_file, &["--json"]);
    let report: serde_json::Value = serde_json::from_slice(&completed.stdout).unwrap();

    assert!(completed.status.success());
    assert_eq!(report["commands"][0]["stdout"], "configured\n");
    assert_eq!(report["summary"]["status"], "PASS");
    assert_eq!(report["summary"]["passed"], 1);
    fs::remove_file(config_file).expect("failed to remove test configuration");
}

