# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build Commands

```bash
cargo build --release          # Production build
cargo make check-format        # Format check (CI runs this)
cargo make clippy              # Lint — canonical blocking gate (== CI)
cargo make test                # Run all tests, including the TCK gate (== CI)
cargo make clippy-grafeo       # Lint with the grafeo feature (== clippy, + --features grafeo)
cargo make test-grafeo         # Run all tests with grafeo, adding the differential oracle
cargo make clippy-turso        # Lint with the turso feature
cargo make test-turso          # Run all tests with turso; the TCK gate runs on TursoStore
cargo make ci-flow             # Everything the merge queue's build job runs, incl. coverage
```

Always prefer `cargo make <task>` over invoking `cargo clippy`/`cargo test` directly.
The `Makefile.toml` tasks are the canonical, `--locked` commands that CI
(`on-push.yml`, and `on-merge.yml`'s `ci-flow` / `ci-grafeo-flow`) runs, so a
clean local run predicts a clean CI run. Plain `cargo clippy`/`cargo test` skip
`--locked` and can pass locally on a resolution CI would reject.

- `cargo make clippy` expands to `cargo clippy --all-targets --workspace --locked
  -- -D warnings`.
- `cargo make test` expands to `cargo test --locked`. Don't use cargo-nextest:
  the TCK runner (`tests/tck`, `harness = false`) has its own `main` and can't
  answer nextest's `--list` probe.
- The Grafeo differential oracle (`tests/oracle.rs`) lives behind the `grafeo`
  Cargo feature, off by default. `cargo make clippy-grafeo` / `cargo make
  test-grafeo` run as their own parallel CI jobs (`clippy-grafeo` in
  `on-push.yml`, `grafeo` in `on-merge.yml`) so the feature doesn't rot.
- The Turso backend (`src/store/turso_store.rs`, `tests/turso.rs`) lives behind
  the `turso` feature, off by default, with the same arrangement: `cargo make
  clippy-turso` / `cargo make test-turso` (`clippy-turso` in `on-push.yml`,
  `turso` in `on-merge.yml`). `test-turso` sets `TCK_STORE=turso`, so the TCK
  baseline must hold on Turso as well as on `PetgraphStore`.
- `on-push.yml` is the fast PR gate (format + clippy). The full suite and
  coverage run in the merge queue (`on-merge.yml`). Both report the single
  `⚡ PR Ready` required check.
- The toolchain is pinned in `rust-toolchain.toml`.
