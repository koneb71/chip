# Contributing to chip

Thanks for your interest in chip! This document covers how to build, test, and
submit changes.

## Prerequisites

- **Rust** (stable, edition 2021) — install via [rustup](https://rustup.rs)
- **Protocol Buffers compiler** (`protoc`) — required to build `chip-proto`
  - macOS: `brew install protobuf`
  - Debian/Ubuntu: `sudo apt-get install -y protobuf-compiler`
- **Docker** (optional) — only needed to run the server end-to-end against
  PostgreSQL, or to build the container image

## Project layout

chip is a Cargo workspace of four crates:

| Crate | Role |
| ----- | ---- |
| `chip-core` | The version-control engine: objects, store, diff, merge, op-log, high-level ops. No network or database. |
| `chip-cli` | The `chip` command-line client (local commands + HTTP/SSH sync). |
| `chip-proto` | gRPC service definitions (compiled from `.proto` via `protoc`). |
| `chip-server` | The deployable host: gRPC sync, axum web UI, PostgreSQL auth, SSH transport. |

**Read [ARCHITECTURE.md](ARCHITECTURE.md) first** — it explains the data model,
the object store, the sync protocol, and how the server is put together. It's the
fastest way to get oriented before changing anything.

## Build & test

```sh
cargo build                  # build everything
cargo test                   # run the test suite
cargo fmt --all              # format
cargo clippy --all-targets --workspace -- -D warnings   # lint (must be clean)
```

Before opening a pull request, make sure all four of the above pass. CI runs the
same checks (`.github/workflows/ci.yml`).

### Running the server locally

The server needs PostgreSQL. The quickest path is Docker Compose:

```sh
docker compose up        # starts Postgres + chip-server
```

See [DEPLOY.md](DEPLOY.md) for production configuration and the full list of
environment variables.

## Coding guidelines

- **Formatting & lints are enforced.** Keep `cargo fmt` clean and `cargo clippy
  -D warnings` green.
- **Match the surrounding style.** Comment density, naming, and error handling
  should look like the code already in the file.
- **Tests for behavior changes.** Engine changes belong in `chip-core` tests;
  prefer small, focused tests over end-to-end ones where possible.
- **No secrets in commits.** Never commit keys, tokens, or `.env` files.
- **Security-sensitive changes** (auth, crypto, input validation, the sync
  protocol) deserve extra care and a clear description of the threat being
  addressed. See [SECURITY.md](SECURITY.md).

## Pull requests

1. Fork and create a topic branch.
2. Make your change with tests and docs.
3. Run fmt, clippy, and the test suite.
4. Open a PR describing **what** changed and **why**. Link any related issue.

By contributing, you agree that your contributions are licensed under the
project's dual [MIT](LICENSE-MIT) / [Apache-2.0](LICENSE-APACHE) license.
