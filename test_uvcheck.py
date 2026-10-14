"""Tests for the uvcheck command-line interface."""

from __future__ import annotations

import json
import os
import subprocess
import tempfile
import unittest
import xml.etree.ElementTree as ET
from pathlib import Path

import uvcheck

UVCHECK = Path(__file__).parent / "uvcheck"


class UvcheckTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary_directory = tempfile.TemporaryDirectory()
        self.state_file = Path(self.temporary_directory.name) / "state"
        self.config_file = Path(self.temporary_directory.name) / ".uvcheck"
        self.environment = os.environ | {
            "UVCHECK_CONFIG_FILE": str(self.config_file),
            "UVCHECK_STATE_FILE": str(self.state_file),
        }

    def tearDown(self) -> None:
        self.temporary_directory.cleanup()

    def run_uvcheck(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [UVCHECK, *arguments],
            check=False,
            capture_output=True,
            env=self.environment,
            text=True,
        )

    def test_session_counts_commands_and_returns_failure(self) -> None:
        self.assertEqual(self.run_uvcheck("--start").returncode, 0)
        self.assertRegex(self.state_file.read_text(encoding="utf-8"), r"^\d+ 0 0\n$")
        self.assertEqual(self.run_uvcheck("python", "-c", "pass").returncode, 0)
        self.assertEqual(self.run_uvcheck("python", "-c", "raise SystemExit(3)").returncode, 3)

        stopped = self.run_uvcheck("--stop")

        self.assertEqual(stopped.returncode, 1)
        self.assertRegex(stopped.stdout, r"FAIL \(1 PASS, 1 FAIL\) in \d+\.\d{2}s")
        self.assertFalse(self.state_file.exists())

    def test_stop_without_active_session_returns_usage_error(self) -> None:
        stopped = self.run_uvcheck("--stop")

        self.assertEqual(stopped.returncode, 2)
        self.assertEqual(stopped.stderr, "uvcheck: no active session\n")

    def test_json_command_report_captures_output_and_status(self) -> None:
        result = self.run_uvcheck(
            "--json",
            "python",
            "-c",
            "import sys; print('ok'); print('note', file=sys.stderr)",
        )

        self.assertEqual(result.returncode, 0)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "PASS")
        self.assertEqual(report["exit_code"], 0)
        self.assertEqual(report["stdout"], "ok\n")
        self.assertEqual(report["stderr"], "note\n")

    def test_json_failed_command_preserves_exit_code(self) -> None:
        result = self.run_uvcheck("--json", "python", "-c", "raise SystemExit(3)")

        self.assertEqual(result.returncode, 3)
        report = json.loads(result.stdout)
        self.assertEqual(report["status"], "FAIL")
        self.assertEqual(report["exit_code"], 3)

    def test_json_configured_report_includes_command_results_and_summary(self) -> None:
        self.config_file.write_text("python -c 'print(\"configured\")'\n", encoding="utf-8")

        result = self.run_uvcheck("--json")

        self.assertEqual(result.returncode, 0)
        report = json.loads(result.stdout)
        self.assertEqual(report["commands"][0]["stdout"], "configured\n")
        self.assertEqual(report["summary"]["status"], "PASS")
        self.assertEqual(report["summary"]["passed"], 1)

    def test_xml_configured_report_includes_logs_and_failures(self) -> None:
        self.config_file.write_text(
            "python -c 'print(\"<configured>\")'\n"
            "python -c 'import sys; print(\"bad & broken\", file=sys.stderr); raise SystemExit(3)'\n",
            encoding="utf-8",
        )
        report_path = self.config_file.parent / "junit.xml"

        result = self.run_uvcheck("--json", "--junit-xml", str(report_path))

        report = ET.parse(report_path).getroot()
        self.assertEqual(result.returncode, 1)
        json_report = json.loads(result.stdout)
        self.assertEqual(json_report["summary"]["failed"], 1)
        self.assertEqual(report.attrib["tests"], "2")
        self.assertEqual(report.attrib["failures"], "1")
        self.assertEqual(report.attrib["errors"], "0")
        self.assertRegex(report.attrib["timestamp"], r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")
        self.assertEqual(report.find("testsuite").attrib["errors"], "0")
        self.assertEqual(report.find("testsuite").attrib["timestamp"], report.attrib["timestamp"])
        cases = report.findall("./testsuite/testcase")
        self.assertEqual(cases[0].findtext("system-out"), "<configured>\n")
        self.assertEqual(cases[1].find("failure").attrib["message"], "Command failed with exit code 3")
        self.assertEqual(cases[1].findtext("system-err"), "bad & broken\n")

    def test_xml_single_command_writes_a_junit_report(self) -> None:
        report_path = self.config_file.parent / "single.xml"

        result = self.run_uvcheck("--json", "--junit-xml", str(report_path), "python", "-c", "print('single')")

        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)["stdout"], "single\n")
        report = ET.parse(report_path).getroot()
        self.assertEqual(report.attrib["tests"], "1")
        self.assertEqual(report.findtext("./testsuite/testcase/system-out"), "single\n")

    def test_xml_requires_a_report_path(self) -> None:
        result = self.run_uvcheck("--json", "--junit-xml")

        self.assertEqual(result.returncode, 2)
        self.assertIn("--junit-xml requires a PATH", result.stderr)

    def test_junit_xml_requires_json_output_mode(self) -> None:
        report_path = self.config_file.parent / "junit.xml"

        result = self.run_uvcheck("--junit-xml", str(report_path))

        self.assertEqual(result.returncode, 2)
        self.assertIn("--junit-xml requires --json", result.stderr)
        self.assertFalse(report_path.exists())

    def test_json_and_junit_xml_write_to_separate_outputs(self) -> None:
        report_path = self.config_file.parent / "combined.xml"
        result = self.run_uvcheck(
            "--json",
            "--junit-xml",
            str(report_path),
            "python",
            "-c",
            "print('combined')",
        )

        self.assertEqual(result.returncode, 0)
        self.assertEqual(json.loads(result.stdout)["stdout"], "combined\n")
        self.assertEqual(
            ET.parse(report_path).getroot().findtext("./testsuite/testcase/system-out"),
            "combined\n",
        )

    def test_help_flags_print_usage_and_succeed(self) -> None:
        for flag in ("--help", "-h"):
            with self.subTest(flag=flag):
                result = self.run_uvcheck(flag)

                self.assertEqual(result.returncode, 0)
                self.assertIn("Usage:", result.stdout)
                self.assertIn("--init", result.stdout)
                self.assertIn("--start", result.stdout)
                self.assertIn("--stop", result.stdout)
                self.assertIn("--junit-xml PATH", result.stdout)

    def test_status_colors_respect_tty_and_no_color(self) -> None:
        self.assertEqual(
            uvcheck.colorize_status("PASS", is_tty=True, no_color=False),
            "\033[32mPASS\033[0m",
        )
        self.assertEqual(
            uvcheck.colorize_status("FAIL", is_tty=True, no_color=False),
            "\033[31mFAIL\033[0m",
        )
        self.assertEqual(uvcheck.colorize_status("PASS", is_tty=False), "PASS")
        self.assertEqual(
            uvcheck.colorize_status("FAIL", is_tty=True, no_color=True),
            "FAIL",
        )

    def test_no_arguments_runs_configured_commands(self) -> None:
        self.config_file.write_text(
            "#!/bin/bash\n"
            "# Keep running after a failure\n"
            "uv run python -c 'raise SystemExit(3)'\n"
            "uv run python -c 'print(\"configured check\")'\n",
            encoding="utf-8",
        )

        completed = self.run_uvcheck()

        self.assertEqual(completed.returncode, 1)
        self.assertIn("configured check", completed.stdout)
        self.assertIn("+ uv run python -c", completed.stdout)
        self.assertRegex(completed.stdout, r"FAIL \(1 PASS, 1 FAIL\) in \d+\.\d{2}s")
        self.assertFalse(self.state_file.exists())

    def test_no_arguments_runs_shorthand_configured_commands_through_uv_run(self) -> None:
        self.config_file.write_text(
            "python -c 'print(\"shorthand configured check\")'\n",
            encoding="utf-8",
        )

        completed = self.run_uvcheck()

        self.assertEqual(completed.returncode, 0)
        self.assertIn("+ uv run python -c", completed.stdout)
        self.assertIn("shorthand configured check", completed.stdout)

    def test_no_arguments_runs_uvx_and_uv_tool_commands(self) -> None:
        self.config_file.write_text(
            "uvx python -c 'print(\"uvx configured check\")'\n"
            "uv tool run python -c 'print(\"uv tool configured check\")'\n",
            encoding="utf-8",
        )

        completed = self.run_uvcheck()

        self.assertEqual(completed.returncode, 0)
        self.assertIn("+ uvx python -c", completed.stdout)
        self.assertIn("uvx configured check", completed.stdout)
        self.assertIn("+ uv tool run python -c", completed.stdout)
        self.assertIn("uv tool configured check", completed.stdout)


if __name__ == "__main__":
    unittest.main()
