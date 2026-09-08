# Contributing to Flash

Thanks for looking. Flash is maintained by one person alongside the
hosted service it powers, so the rules below exist to keep review
cheap for both of us.

## Before you start

- Open an issue first for anything beyond a small fix, so we agree on
  the shape before you write it. Scheduling behaviour (FSRS, queue
  order, daily limits) and anything touching accounts or tokens gets
  extra scrutiny.
- Contributions require the contributor license agreement in
  [CLA.md](CLA.md). A bot asks you to sign it on your first pull
  request; it is a one-time click.

## Building and testing

```
cargo test --workspace
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
```

All three must be clean; CI runs them on every pull request. On Windows
the vendored OpenSSL needs Strawberry Perl and NASM on `PATH`; Linux and
macOS need nothing beyond a Rust toolchain.

The layout, briefly:

- `crates/flash-core` — pure domain code, no IO. Scheduler reference
  vectors live in `tests/fsrs_vectors.json`; don't regenerate them
  casually.
- `crates/flash-store` — every SQL statement. Migrations are append-only:
  never edit a shipped one, add the next slot.
- `crates/flash-server` — HTTP. Handlers stay thin; every rule lives in
  `service.rs` or a `flows` module so the web UI, the JSON API and the
  MCP tools can't drift apart. Templates are askama, behaviour is htmx.

Tests are integration-style and run against an in-memory database; look
at `crates/flash-server/tests/` for the harness (`common::*`).

## Pull requests

- One change per pull request, with tests that would fail without it.
- Explain the behaviour change in the description; commit messages
  should say why, not just what.
- Keep the diff free of formatting churn and unrelated refactors.

This repository is a read-only mirror of a private monorepo that also
holds the hosted product. Merged pull requests are applied to that
repository with `git am`, credited to you, and appear here with the next
sync; the pull request is then closed with a pointer to the sync commit.
That is why history here arrives in batches rather than merge commits.

## Reporting security issues

Please do not open a public issue for anything that could affect other
people's data. Email the address on the hosted service's support page
and allow a few days for a reply.
