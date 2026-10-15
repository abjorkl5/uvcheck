# uvcheck

`uvcheck` is a small project wrapper for running checks through [uv](https://docs.astral.sh/uv).

## Installation

From the repository root, choose either implementation. Both provide the
`uvcheck` command and require `uv` on `PATH` when running project checks.

### Python

Install the Python implementation with:

```shell
uv tool install .
```

### Rust

With the Rust toolchain installed, build and install the Rust implementation
with:

```shell
cargo install --path uvcheck-rs
```

## Development setup

This repository uses `uv` to manage Ruff as a development dependency. Set up the
environment with:

```shell
uv sync
```

The repository configures Ruff in `pyproject.toml` with a 120-character line
length and the default `E4`, `E7`, `E9`, and `F` rules. Run the checks with:

```shell
uv run ruff format --check
uv run ruff check
uv run python -m unittest -b
```

With no `.uvcheck` file, `uvcheck` runs these same checks.

## Ruff defaults

`uvcheck` is agnostic about Ruff policy. Each project can configure Ruff in its
`pyproject.toml`, including the line length and enabled rules. Without a
project override, Ruff uses an 88-character line length, matching Black, and
its default rule selection is:

```text
E4  Import-related pycodestyle errors
E7  Statement-related pycodestyle errors
E9  Runtime-related pycodestyle errors
F   Pyflakes rules
```

Those defaults can be written explicitly in `pyproject.toml` as:

```toml
[tool.ruff]
line-length = 88

[tool.ruff.lint]
select = ["E4", "E7", "E9", "F"]
```

Projects can enable additional rules with Ruff configuration such as
`extend-select`; `uvcheck` runs the configured `ruff check` without changing
that policy.

## Interface

```shell
uvcheck
uvcheck COMMAND [ARGUMENT ...]
uvcheck --json
uvcheck --json COMMAND [ARGUMENT ...]
uvcheck --json --junit-xml REPORT.xml [COMMAND [ARGUMENT ...]]
uvcheck --help
uvcheck --init
uvcheck --start
uvcheck --stop
```

With no arguments, `uvcheck` runs the configured checks for the project.
Use `uvcheck --help` for a summary of the command-line interface.
If `.uvcheck` is present, it reads each command line and runs unprefixed lines
through `uv run`. It also accepts explicit `uv run COMMAND`, `uvx COMMAND`, or
`uv tool run COMMAND` lines and preserves the selected launcher. All commands
run through an automatically started and stopped session. If no config file is
present, it falls back to a built-in
standard set of checks for a typical `uv` project:

```text
# Formatting
uv run ruff format --check
# Linting
uv run ruff check
# Unit Testing
uv run python -m unittest -b
```

Run `uvcheck --init` to create the config file with the built-in default
configuration. The default path is `.uvcheck`; if the config file already
exists, `uvcheck` reports that and leaves the existing file unchanged.

The built-in configuration is intentionally small. Projects can extend the
generated `.uvcheck` file with optional checks when the corresponding tools are
part of the project's development dependencies:

```shell
uv add --dev ty
```

```text
uv run ruff format --check
uv run ruff check
uv run ty check
uv run python -m unittest -b
```

Use one test runner for a project, not both. To use pytest instead of the
standard-library test runner, add it as a development dependency and replace
the unittest command:

```shell
uv add --dev pytest
```

```text
uv run pytest -q
```

**Test runner flags:** `-b` buffers unittest output and shows it when a test
fails. The pytest `-q` flag keeps the runner output concise so `uvcheck` can
report the test result and timing cleanly.

## Fixing checks

`uvcheck` only validates files; it does not modify them. To fix Ruff findings,
use the separate repair workflow:

```shell
uv run ruff check --fix
uv run ruff format
```

Lint fixes can remove imports or change statements, so format the resulting
code afterward. Run `uvcheck` once the fixes are complete to verify the
project without modifying it.

The generated file can also be run directly, with `bash -ex .uvcheck`, because
its commands use explicit `uv run` launchers. If you add an unprefixed command
to a custom `.uvcheck`, run it through `uvcheck` so the implicit `uv run`
launcher is applied. Running such a file directly with Bash requires each tool
to be available on `PATH`, for example by installing it with `uv tool install`
and running `uv tool update-shell`; it may then use a different version from
the project's development dependency. This is the usual difference between
running `uv run ruff` and running `ruff` directly, rather than behavior unique
to `uvcheck`.

Set `UVCHECK_CONFIG_FILE` to override the config path explicitly. When that
path is missing or unreadable, `uvcheck` reports the I/O or config error
instead of silently falling back to the built-in defaults.

The wrapper runs each configured command using its selected launcher. An
unprefixed command implies `uv run`:

```shell
ruff check
```

is equivalent to:

```shell
uv run COMMAND [ARGUMENT ...]
uv tool run COMMAND [ARGUMENT ...]
uvx COMMAND [ARGUMENT ...]
```

Use `uv run` for commands installed in the project's environment. Use
`uv tool run` or its `uvx` shorthand for running a tool in an isolated tool
environment.

For example:

```shell
uvcheck ruff format --check
uvcheck ruff check
uvcheck ty check
uvcheck python -m unittest -b
```

Use `--start` and `--stop` to time and summarize a group of checks:

```shell
uvcheck --start
uvcheck ruff format --check || :
uvcheck ruff check || :
uvcheck --stop
```

Every `uvcheck` command run by the same parent process between these calls is
included in the final PASS and FAIL counters. `--stop` exits with status 1 if
any command failed, or status 2 if there is no active session.

Pass `--json` before a single check, or by itself to run the configured checks,
to emit one JSON object on stdout. Command reports include the argument array,
`PASS` or `FAIL` status, exit code, elapsed seconds, and captured stdout and
stderr. Configured-check reports also include a `summary` object with the
aggregate status, pass/fail counts, and duration. The `--init`, `--start`, and
`--stop` commands remain human-readable.

Pass `--json --junit-xml PATH` to write a JUnit XML report to a file while
running the configured checks, or place a command after the path to report a
single check. `--junit-xml` requires `--json`.
Each command is represented by a testcase with its duration, captured stdout
and stderr, and a failure entry for nonzero exit codes. The suite timestamp is
UTC in ISO-8601 format. The XML report uses two-space indentation and a UTF-8
declaration. JSON is emitted on stdout, and the command's exit status remains
unchanged.

## Behavior

- Prints the command before running it.
- Sets `NO_COLOR=1` in the child process environment.
- Reports `PASS` or `FAIL` and elapsed time, using dark green for `PASS` and dark red for `FAIL` when stdout is a
  TTY and `NO_COLOR` is not set.
- Preserves the wrapped command's exit code.
- Streams progress characters while `python -m unittest -b` or `pytest -q` run.
- Suppresses unittest's separator and extra blank line, and pytest's wrapped `[ NN%]` markers.
- Combines the test status and timing into one line, for example:

  ```text
  OK - Ran 25 tests in 1.12s
  PASS python -m unittest -b (2.35s)
  ```

  ```text
  25 passed in 1.12s
  PASS pytest -q (2.35s)
  ```

- Preserves test diagnostics when the test command fails.
- The `pytest -q` special case only handles plain pytest output, without
  plugins that change the progress format.

## Implementation

The active implementation is `uvcheck.py`, invoked through the small `uvcheck`
shell launcher. The `uvcheck-rs` directory contains a Rust implementation with
the same command interface and behavior.

The command-line interface is the contract, so the implementation can be
rewritten in Rust later without changing `.uvcheck`.
