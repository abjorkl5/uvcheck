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

#[test]
fn junit_xml_configured_report_includes_logs_and_failures() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    let report_file = temporary_path("junit.xml");
    fs::write(
        &config_file,
        "python -c 'print(\"<configured>\")'\n\
         python -c 'import sys; print(\"bad & broken\", file=sys.stderr); raise SystemExit(3)'\n",
    )
    .expect("failed to write test configuration");

    let completed = run_uvcheck(
        &state_file,
        &config_file,
        &["--json", "--junit-xml", report_file.to_str().unwrap()],
    );
    let xml = fs::read_to_string(&report_file).expect("failed to read JUnit report");
    let mut reader = quick_xml::Reader::from_str(&xml);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(error) => panic!("invalid JUnit XML: {error}"),
        }
    }
    assert_eq!(completed.status.code(), Some(1));
    assert!(xml.contains("tests=\"2\""));
    assert!(xml.contains("failures=\"1\""));
    assert!(xml.contains("errors=\"0\""));
    let timestamps = xml
        .match_indices("timestamp=\"")
        .map(|(start, _)| {
            xml[start + "timestamp=\"".len()..]
                .split('\"')
                .next()
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(timestamps.len(), 2);
    assert_eq!(timestamps[0], timestamps[1]);
    assert!(timestamps[0].ends_with('Z'));
    assert!(xml.contains("<system-out>&lt;configured&gt;"));
    assert!(xml.contains("<failure message=\"Command failed with exit code 3\">"));
    assert!(xml.contains("bad &amp; broken"));

    fs::remove_file(config_file).expect("failed to remove test configuration");
    fs::remove_file(report_file).expect("failed to remove JUnit report");
}

#[test]
fn junit_xml_single_command_writes_a_report_file() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    let report_file = temporary_path("single.xml");

    let completed = run_uvcheck(
        &state_file,
        &config_file,
        &[
            "--json",
            "--junit-xml",
            report_file.to_str().unwrap(),
            "python",
            "-c",
            "print('single')",
        ],
    );

    assert!(completed.status.success());
    let xml = fs::read_to_string(&report_file).expect("failed to read JUnit report");
    assert!(xml.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"));
    assert!(xml.contains("\n  <testsuite "));
    assert!(xml.contains("\n    <testcase "));
    assert!(xml.contains("tests=\"1\""));
    assert!(xml.contains("<system-out>single"));
    fs::remove_file(report_file).expect("failed to remove JUnit report");
}

#[test]
fn json_and_junit_xml_write_to_separate_outputs() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    let report_file = temporary_path("combined.xml");

    let completed = run_uvcheck(
        &state_file,
        &config_file,
        &[
            "--json",
            "--junit-xml",
            report_file.to_str().unwrap(),
            "python",
            "-c",
            "print('combined')",
        ],
    );

    let report: serde_json::Value = serde_json::from_slice(&completed.stdout).unwrap();
    assert!(completed.status.success());
    assert_eq!(report["stdout"], "combined\n");
    let xml = fs::read_to_string(&report_file).expect("failed to read JUnit report");
    assert!(xml.contains("errors=\"0\""));
    assert!(xml.contains("<system-out>combined"));
    fs::remove_file(report_file).expect("failed to remove JUnit report");
}

#[test]
fn junit_xml_requires_json_output_mode() {
    let state_file = temporary_path("state");
    let config_file = temporary_path("config");
    let report_file = temporary_path("junit.xml");

    let completed = run_uvcheck(
        &state_file,
        &config_file,
        &["--junit-xml", report_file.to_str().unwrap()],
    );

    assert_eq!(completed.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&completed.stderr).contains("--junit-xml requires --json"));
    assert!(!report_file.exists());
}
