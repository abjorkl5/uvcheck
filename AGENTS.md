# Agent Instructions

- Treat [README.md](README.md) as the source of truth for the CLI contract, config file format, and expected
  behavior. Read it before changing runtime behavior.
- The user-facing entry point is [uvcheck](uvcheck). The active implementation is [uvcheck.py](uvcheck.py);
  [uvcheck-rs](uvcheck-rs) is a parallel Rust implementation of the same interface. Keep the observable
  behavior aligned when changing either implementation.
- Prefer minimal, targeted exploration before patching. Keep changes scoped to the documented contract unless a
  README update is part of the fix.
- Validate changes with the repo’s normal commands:
  - Full configured checks: `./uvcheck`
  - Python tests: `python -m unittest -b`
  - Rust tests: `cargo test` from [uvcheck-rs](uvcheck-rs)
  - Ruff checks: `uv run ruff format --check` and `uv run ruff check`
  - Markdown check: `uvx mdlint check`
- Follow the project’s conventions:
  - Use `uv` for project-local dev tooling and execution.
  - Keep packaging and dependencies minimal; do not add build-system machinery for this small script without a
    concrete requirement.
  - Respect `.uvcheck` as project configuration rather than inventing new behavior or silently altering config
    semantics.
  - `.uvcheck` accepts shell-quoted commands; unprefixed commands imply `uv run`, while explicit `uvx` and `uv tool
    run` launchers are also supported. Comments and blank lines are ignored.
  - `uvcheck` validates checks; it does not repair code. Use Ruff’s fix/format commands separately when
    addressing findings.
  - Keep `-b` for unittest and `-q` for pytest configurations: the wrapper recognizes these flags to present
    compact test progress and summaries.
- If a change affects command-line semantics or output, verify both implementations still behave equivalently
  from the user’s perspective.
- For changes to `--start`/`--stop`, preserve the shared state-file format (`started_at passed failed`), locking,
  and exit-code behavior so Python and Rust processes can participate in the same session.
