# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project

LAN chat TUI in Rust. mDNS + TCP, newline-delimited JSON wire protocol, ratatui UI, tokio runtime. Single binary acts as server and client.

## Quality gates (must be green before commit/PR)

Run `/verify` to check all three at once:

- `cargo fmt --check` — formatting must match rustfmt defaults
- `cargo clippy --all-targets -- -D warnings` — no clippy warnings
- `cargo test` — all unit + integration tests pass

The `tests/loopback.rs` integration test spins real `TcpStream` pairs on `127.0.0.1`. It is the canonical end-to-end check for the handshake; do not skip it when you change `src/net/`.

## Repo etiquette

- Commit subjects: short, imperative, no Conventional Commits prefix. Match existing log style ("Gitignore update", "User dedup", "Fix dial EINVAL for link-local IPv6 peers").
- Never `--no-verify` on git commands.

## Persistent state on disk

The binary writes to `~/.local/share/cweldrop/`:
- `identity.key` — ed25519 secret seed, mode 0600. Generated once per user account, reused across runs.
- `identity.pub` — public key, mode 0644.
- `history/<pubkey_hex>.jsonl` — per-peer chat transcripts.

Tests must NOT use `Identity::load_or_create()` (which touches the real `~/.local/share/cweldrop/`). Use `Identity::load_or_create_in(&tmp_path)` with a unique temp dir per test.

## Logging

`tracing` writes to `cweldrop.log` in the cwd. Stdout/stderr are reserved for the TUI — never `println!`/`eprintln!` from library code or it corrupts the display.
