---
name: verify
description: Run the full pre-commit gate for cweldrop — cargo fmt, clippy (warnings as errors), and the full test suite (unit + tests/loopback.rs integration). Use before committing, before opening a PR, or when the user says "verify", "check", or "is it green".
---

Run these three commands in order. Stop and report on the first failure — do not proceed.

1. `cargo fmt --all -- --check`
2. `cargo clippy --all-targets -- -D warnings`
3. `cargo test`

On success: report "verify: green" plus the test count from step 3.

On failure: quote the exact failing output (rustc/clippy error, panicking test name + assertion). Do not attempt to auto-fix — let the user decide.

Notes:
- Step 3 includes `tests/loopback.rs`, which binds real TCP sockets on `127.0.0.1`. If it hangs, suspect a leftover process on the same port from a prior crash.
- These commands match the gates listed in `CLAUDE.md`. Keep them in sync.
