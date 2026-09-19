# Contributing to sap-for-agents

[English](./CONTRIBUTING.md) | [简体中文](./CONTRIBUTING.zh-CN.md)

Thanks for your interest! This guide covers the practical bits of working on this repo.

## Prerequisites

- **Rust** (stable toolchain; see `rust-version` in `Cargo.toml` for the MSRV)
- **SAP NWRFC SDK** — SAP's proprietary C library, downloaded from SAP Support Portal
  (cannot be redistributed). Drop the platform zip into `nwrfcsdk/lib/<any-dir>/`
  and `./start.sh` auto-extracts it; see README §Quick Start.
- Optional: a **local SAP trial system** (e.g. the ABAP Cloud Developer Trial docker
  image) for integration testing.

## Building & testing

```bash
# Compile + unit tests (no SAP needed — CI does exactly this)
cargo test

# With a real SAP system configured in .env, also run the integration suite
# (tests/*.rs are marked #[ignore]; they spawn the real server binary):
DYLD_LIBRARY_PATH=./nwrfcsdk/lib/darwin-aarch64 cargo test -- --ignored   # macOS example
# Linux: LD_LIBRARY_PATH=./nwrfcsdk/lib/linux-x86_64

# Lint gate (CI runs this with -D warnings)
cargo clippy --all-targets -- -D warnings
```

Notes on the integration suite:

- Each test spawns the real HTTP server on a unique port and talks to it over HTTP.
- The suite covers all endpoint families incl. OpenAPI, MCP (`POST /mcp`) and the
  connection-pool behavior. Some environment-dependent results (e.g. the where-used
  index on trial systems) are asserted structurally, not by content.
- The SAP integration tests are skipped automatically when the connection env vars
  are missing — they never fail CI.

## Design conventions

- **Pragmatic hand-rolled over framework magic**: the OpenAPI spec is a data-driven
  JSON literal (`src/openapi.rs`), the MCP server is a hand-written JSON-RPC subset
  (`src/mcp.rs`) — no utoipa/rmcp dependencies. Match that style unless there's a
  strong reason not to.
- **Docs are part of the feature**: README.md / README.zh-CN.md / AGENTS.md /
  AGENTS.zh-CN.md / homepage (`src/index*.html`) must stay in sync in both languages
  when endpoints or behavior change.
- **Gaps stay visible**: when a sub-result can't be fetched (e.g. one BAPI's metadata
  in the dynamic spec, one dependency in the source prologue), surface the error in
  place instead of dropping it silently.
- **Tests gate releases**: new endpoints need integration tests; pure logic needs
  unit tests. `cargo test` green locally (incl. `--ignored` when SAP is reachable)
  before submitting.

## Submitting

1. Fork / branch from `main`.
2. `cargo test && cargo clippy --all-targets -- -D warnings` must pass.
3. PR with a clear description; bilingual doc updates included when user-facing.

## Releasing (maintainers)

See README §10 — bump `Cargo.toml`, tag `vX.Y.Z`, push the tag; CI builds the five
platform binaries and attaches them to the GitHub Release automatically.
