#!/usr/bin/env python3
"""Run a project command through uv and print a compact result."""

from __future__ import annotations

import fcntl
import json
import os
import re
import selectors
import shlex
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET
from collections.abc import Sequence
from datetime import datetime, timezone
from pathlib import Path
from typing import Literal, TextIO

Status = Literal["PASS", "FAIL"]


STATE_FILE = Path(
    os.environ.get(
        "UVCHECK_STATE_FILE",
        Path(tempfile.gettempdir()) / f"uvcheck-{os.getuid()}-{os.getppid()}.state",
    ),
)
CONFIG_FILE = Path(os.environ.get("UVCHECK_CONFIG_FILE", ".uvcheck"))
DEFAULT_COMMANDS = [
    ["ruff", "format", "--check"],
    ["ruff", "check"],
    ["python", "-m", "unittest", "-b"],
]
DEFAULT_RUNNER = ["uv", "run"]
CONFIG_RUNNERS = (["uv", "run"], ["uvx"], ["uv", "tool", "run"])
HELP = """\
Usage:
  uvcheck
  uvcheck --json [COMMAND [ARGUMENT ...]]
  uvcheck --json --junit-xml PATH [COMMAND [ARGUMENT ...]]
  uvcheck COMMAND [ARGUMENT ...]

Options:
  --help, -h  Show this help message
  --init      Create the configuration file
  --start     Start a shared check session
  --stop      Stop a shared check session and print its summary
  --json      Emit JSON output for check runs
  --junit-xml PATH  Write a JUnit XML report to PATH (requires --json)
"""


def colorize_status(status: str, *, is_tty: bool | None = None, no_color: bool | None = None) -> str:
    if is_tty is None:
        is_tty = sys.stdout.isatty() and os.environ.get("TERM") != "dumb"
    if no_color is None:
        no_color = "NO_COLOR" in os.environ
    if not is_tty or no_color:
        return status
    palette = {"PASS": "\033[32m", "FAIL": "\033[31m"}
    color = palette.get(status)
    if color is None:
        return status
    return f"{color}{status}\033[0m"


def start_session() -> int:
    with STATE_FILE.open("w", encoding="utf-8") as state_file:
        fcntl.flock(state_file, fcntl.LOCK_EX)
        write_state(state_file, (time.time_ns(), 0, 0))
    return 0


def read_state(state_file: TextIO) -> tuple[int, int, int]:
    values = state_file.read().split()
    if len(values) != 3:
        raise ValueError("invalid uvcheck state")
    return int(values[0]), int(values[1]), int(values[2])


def write_state(state_file: TextIO, state: tuple[int, int, int]) -> None:
    state_file.seek(0)
    state_file.write(f"{state[0]} {state[1]} {state[2]}\n")
    state_file.truncate()


def update_session(returncode: int) -> None:
    try:
        with STATE_FILE.open("r+", encoding="utf-8") as state_file:
            fcntl.flock(state_file, fcntl.LOCK_EX)
            started_at, passed, failed = read_state(state_file)
            if returncode == 0:
                passed += 1
            else:
                failed += 1
            write_state(state_file, (started_at, passed, failed))
    except FileNotFoundError:
        pass


def stop_session(*, json_output: bool = False, commands: list[dict[str, object]] | None = None) -> int:
    try:
        with STATE_FILE.open(encoding="utf-8") as state_file:
            fcntl.flock(state_file, fcntl.LOCK_EX)
            started_at, passed, failed = read_state(state_file)
            STATE_FILE.unlink()
    except FileNotFoundError:
        if json_output:
            print(json.dumps({"error": "no active session", "exit_code": 2}))
            return 2
        print("uvcheck: no active session", file=sys.stderr)
        return 2

    elapsed = (time.time_ns() - started_at) / 1_000_000_000
    status: Status = "PASS" if failed == 0 else "FAIL"
    summary: dict[str, object] = {
        "status": status,
        "passed": passed,
        "failed": failed,
        "duration_seconds": elapsed,
    }
    if json_output:
        report: dict[str, object] = {"summary": summary}
        if commands is not None:
            report["commands"] = commands
        print(json.dumps(report))
    else:
        colored_status = colorize_status(status)
        print(f"\n{colored_status} ({passed} PASS, {failed} FAIL) in {elapsed:.2f}s")
    return 0 if failed == 0 else 1


def default_commands() -> list[tuple[list[str], list[str]]]:
    return [(DEFAULT_RUNNER, command.copy()) for command in DEFAULT_COMMANDS]


def default_config() -> str:
    return (
        "# Formatting\n"
        "uv run ruff format --check\n"
        "# Linting\n"
        "uv run ruff check\n"
        "# Unit tests\n"
        "uv run python -m unittest -b\n"
    )


def init_config() -> int:
    try:
        with CONFIG_FILE.open("x", encoding="utf-8") as config_file:
            config_file.write(default_config())
    except FileExistsError:
        print(f"uvcheck: configuration already exists: {CONFIG_FILE}", file=sys.stderr)
        return 2
    except OSError as error:
        print(f"uvcheck: {CONFIG_FILE}: {error}", file=sys.stderr)
        return 2

    print(f"uvcheck: created {CONFIG_FILE}")
    return 0


def junit_timestamp() -> str:
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def junit_xml_report(results: list[dict[str, object]], timestamp: str) -> bytes:
    tests = len(results)
    failures = sum(result["status"] == "FAIL" for result in results)
    elapsed = 0.0
    for result in results:
        duration = result["duration_seconds"]
        if not isinstance(duration, (int, float)):
            raise TypeError("command duration must be numeric")
        elapsed += duration
    attributes = {
        "name": "uvcheck",
        "tests": str(tests),
        "failures": str(failures),
        "errors": "0",
        "time": f"{elapsed:.6f}",
        "timestamp": timestamp,
    }
    suites = ET.Element("testsuites", attributes)
    suite = ET.SubElement(suites, "testsuite", attributes)
    for result in results:
        command = result["command"]
        assert isinstance(command, list)
        duration = result["duration_seconds"]
        if not isinstance(duration, (int, float)):
            raise TypeError("command duration must be numeric")
        case = ET.SubElement(
            suite,
            "testcase",
            {
                "classname": "uvcheck",
                "name": shlex.join(command),
                "time": f"{duration:.6f}",
            },
        )
        if result["status"] == "FAIL":
            failure = ET.SubElement(
                case,
                "failure",
                {"message": f"Command failed with exit code {result['exit_code']}"},
            )
            failure.text = f"Command failed with exit code {result['exit_code']}"
        for key, tag in (("stdout", "system-out"), ("stderr", "system-err")):
            output = result[key]
            if output:
                ET.SubElement(case, tag).text = str(output)
    return ET.tostring(suites, encoding="utf-8", xml_declaration=True)


def write_junit_xml(path: Path, results: list[dict[str, object]], timestamp: str) -> int:
    try:
        path.write_bytes(junit_xml_report(results, timestamp))
    except OSError as error:
        print(f"uvcheck: {path}: {error}", file=sys.stderr)
        return 2
    return 0


def run_all(*, json_output: bool = False, xml_path: Path | None = None) -> int:
    has_explicit_config = "UVCHECK_CONFIG_FILE" in os.environ
    try:
        lines = CONFIG_FILE.read_text(encoding="utf-8").splitlines()
    except FileNotFoundError:
        if has_explicit_config:
            if json_output:
                print(json.dumps({"error": f"configuration not found: {CONFIG_FILE}", "exit_code": 2}))
                return 2
            print(f"uvcheck: configuration not found: {CONFIG_FILE}", file=sys.stderr)
            return 2
        commands = default_commands()
    except OSError as error:
        if json_output:
            print(json.dumps({"error": f"{CONFIG_FILE}: {error}", "exit_code": 2}))
            return 2
        print(f"uvcheck: {CONFIG_FILE}: {error}", file=sys.stderr)
        return 2
    else:
        commands = []
        for line_number, line in enumerate(lines, start=1):
            command = shlex.split(line, comments=True)
            if not command:
                continue
            explicit_runner = next(
                (candidate for candidate in CONFIG_RUNNERS if command[: len(candidate)] == candidate),
                None,
            )
            if explicit_runner is not None and len(command) == len(explicit_runner):
                print(
                    f"uvcheck: {CONFIG_FILE}:{line_number}: expected 'uv run COMMAND', "
                    "'uvx COMMAND', or 'uv tool run COMMAND'",
                    file=sys.stderr,
                )
                return 2
            runner = explicit_runner or DEFAULT_RUNNER
            command_start = len(runner) if explicit_runner is not None else 0
            commands.append((runner, command[command_start:]))

    xml_timestamp = junit_timestamp() if xml_path is not None else None
    start_session()
    results = []
    for runner, command in commands:
        _, result = run(command, runner, json_output=json_output, xml_output=xml_path is not None)
        if result is not None:
            results.append(result)
    status = stop_session(json_output=json_output, commands=results if json_output else None)
    if xml_path is not None:
        assert xml_timestamp is not None
        if write_junit_xml(xml_path, results, xml_timestamp) != 0:
            return 2
    return status


PROGRESS_CHARS = b".FEsxXpr"


class ProgressFilter:
    """Streams unittest's `.FEs` or pytest's `.FEsxXpr` progress characters.

    Swallows pytest's trailing "[ NN%]" marker so the dots stay on one line.
    """

    def __init__(self) -> None:
        """Start in the initial dots-scanning state."""
        self.active = True
        self.started = False
        self._state = "dots"

    def consume(self, byte: int) -> bool:
        """Process one progress-stream byte; return whether to keep it in the buffered output."""
        if not self.active:
            return True
        char = chr(byte)
        if self._state == "dots":
            return self._consume_dots(byte, char)
        if self._state == "pad":
            return self._consume_pad(char)
        if self._state == "bracket":
            if char == "]":
                self._state = "after_bracket"
            return False
        return self._consume_after_bracket(char)

    def _consume_dots(self, byte: int, char: str) -> bool:
        if byte in PROGRESS_CHARS:
            print(char, end="", flush=True)
            self.started = True
            return False
        if char == " " and self.started:
            self._state = "pad"
            return False
        if char == "\n" and self.started:
            self.active = False
            return False
        self.active = False
        return True

    def _consume_pad(self, char: str) -> bool:
        if char == " ":
            return False
        if char == "[":
            self._state = "bracket"
            return False
        self.active = False
        return True

    def _consume_after_bracket(self, char: str) -> bool:
        if char == "\n":
            self._state = "dots"
            return False
        self.active = False
        return True


def run_with_progress(
    command: Sequence[str],
    runner: Sequence[str],
    environment: dict[str, str],
    progress_stream: str,
) -> tuple[str, str, int, bool]:
    process = subprocess.Popen(
        [*runner, *command],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        env=environment,
    )
    assert process.stdout is not None
    assert process.stderr is not None
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ, "stdout")
    selector.register(process.stderr, selectors.EVENT_READ, "stderr")
    stdout_chunks: list[bytes] = []
    stderr_chunks: list[bytes] = []
    progress = ProgressFilter()

    while selector.get_map():
        for key, _ in selector.select():
            data = os.read(key.fd, 4096)
            if not data:
                selector.unregister(key.fileobj)
                continue
            chunks = stdout_chunks if key.data == "stdout" else stderr_chunks
            if key.data != progress_stream or not progress.active:
                chunks.append(data)
                continue
            for byte in data:
                if progress.consume(byte):
                    chunks.append(bytes([byte]))

    return (
        b"".join(stdout_chunks).decode(errors="replace"),
        b"".join(stderr_chunks).decode(errors="replace"),
        process.wait(),
        progress.started,
    )


def is_unittest_command(command: Sequence[str]) -> bool:
    return command[:2] == ["python", "-m"] and "unittest" in command and "-b" in command


def is_pytest_command(command: Sequence[str]) -> bool:
    return command[:1] == ["pytest"] and "-q" in command


def clear_progress_line() -> None:
    if sys.stdout.isatty() and os.environ.get("TERM") != "dumb":
        print("\r\033[2K", end="")
    else:
        print()


def print_test_summary(stdout: str, stderr: str, returncode: int, is_pytest: bool) -> None:
    output = f"{stdout}\n{stderr}"
    if is_pytest:
        test_summary = re.search(r"^\d+ \w[\w, ]* in [\d.]+s$", output, re.MULTILINE)
        test_status = test_summary
    else:
        test_summary = re.search(r"^Ran \d+ tests? in .*$", output, re.MULTILINE)
        test_status = re.search(r"^(OK|FAILED(?: \([^\n]+\))?)$", output, re.MULTILINE)

    if returncode != 0:
        if stdout:
            print(stdout, end="", file=sys.stdout)
        if stderr:
            print(stderr, end="", file=sys.stderr)
    if not (test_summary and test_status):
        return
    if is_pytest:
        print(test_summary.group(0))
    else:
        print(f"{test_status.group(1)} - {test_summary.group(0)}")


def run(
    command: Sequence[str],
    runner: Sequence[str] = DEFAULT_RUNNER,
    *,
    json_output: bool = False,
    xml_output: bool = False,
) -> tuple[int, dict[str, object] | None]:
    started = time.perf_counter()
    is_unittest = is_unittest_command(command)
    is_pytest = is_pytest_command(command)
    is_tracked = is_unittest or is_pytest
    environment = os.environ | {"NO_COLOR": "1"}
    if not json_output:
        print(f"+ {shlex.join(runner)} {shlex.join(command)}", flush=True)

    if json_output or xml_output:
        completed = subprocess.run([*runner, *command], check=False, capture_output=True, text=True, env=environment)
        stdout, stderr, returncode = completed.stdout, completed.stderr, completed.returncode
    elif is_tracked:
        progress_stream = "stderr" if is_unittest else "stdout"
        stdout, stderr, returncode, progress_started = run_with_progress(command, runner, environment, progress_stream)
    else:
        completed = subprocess.run([*runner, *command], check=False, env=environment)
        returncode = completed.returncode

    elapsed = time.perf_counter() - started
    status: Status = "PASS" if returncode == 0 else "FAIL"

    if xml_output and not json_output:
        if stdout:
            print(stdout, end="", file=sys.stdout)
        if stderr:
            print(stderr, end="", file=sys.stderr)
    if is_tracked and not json_output and not xml_output:
        if progress_started:
            clear_progress_line()
        print_test_summary(stdout, stderr, returncode, is_pytest)

    result: dict[str, object] | None = (
        {
            "command": [*runner, *command],
            "status": status,
            "exit_code": returncode,
            "duration_seconds": elapsed,
            "stdout": stdout if json_output else "",
            "stderr": stderr if json_output else "",
        }
        if json_output
        else None
    )
    if not json_output:
        print(f"{colorize_status(status)} {' '.join(command)} ({elapsed:.2f}s)")
    update_session(returncode)
    return returncode, result


def main() -> int:
    arguments = sys.argv[1:]
    json_output = arguments[:1] == ["--json"]
    if json_output:
        arguments = arguments[1:]
    xml_path = None
    if arguments[:1] == ["--junit-xml"]:
        if len(arguments) < 2:
            print("uvcheck: --junit-xml requires a PATH", file=sys.stderr)
            return 2
        xml_path = Path(arguments[1])
        arguments = arguments[2:]
    if xml_path is not None and not json_output:
        print("uvcheck: --junit-xml requires --json", file=sys.stderr)
        return 2
    if not arguments:
        return run_all(json_output=json_output, xml_path=xml_path)
    if arguments in (["--help"], ["-h"]):
        print(HELP, end="")
        return 0
    if arguments == ["--init"]:
        return init_config()
    if arguments == ["--start"]:
        return start_session()
    if arguments == ["--stop"]:
        return stop_session()
    xml_timestamp = junit_timestamp() if xml_path is not None else None
    returncode, result = run(arguments, json_output=json_output, xml_output=xml_path is not None)
    if xml_path is not None and result is not None:
        assert xml_timestamp is not None
        if write_junit_xml(xml_path, [result], xml_timestamp) != 0:
            return 2
    if result is not None and (xml_path is None or json_output):
        print(json.dumps(result))
    return returncode


if __name__ == "__main__":
    sys.exit(main())
