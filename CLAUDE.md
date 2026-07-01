# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What chip is

chip is a changeset-oriented version control system — a Git *alternative* (not a clone) written from scratch in Rust — plus a deployable multi-user server. The model deliberately departs from Git in ways that are load-bearing throughout the code; treat these as invariants, not accidents:

- **No staging area.** `commit` snapshots the whole working tree; there is no `add`.
- **Two identities per commit.** An `ObjectId` (BLAKE3 hash of canonical bytes; content-addressed, changes with content) is distinct from a `ChangeId` (random 48-bit / 12-hex value that is *carried forward* across `amend`/`rebase` and never changes). Anything rewriting a commit must preserve its `ChangeId`.
- **First-class conflicts.** A conflicting merge never aborts — it produces a conflicted commit (`Commit.conflicts` non-empty) you keep working with, then resolve via a normal commit.
- **Universal undo.** Every mutating operation records prior state in the op-log (`oplog`); `chip undo` reverses the last one. New mutating commands must record an op.
- **Content-addressed, pluggable store.** Objects are BLAKE3-addressed and zstd-compressed behind a trait, so the engine runs unchanged against a local dir, an encrypted dir, or S3.

Read **ARCHITECTURE.md** before making non-trivial engine or protocol changes — it is the authoritative deep-dive on the data model, object store, encryption, sync protocol, and server.

## Working rules

These govern *how* to work in this repo, and take precedence over default behavior:

1. **Plan before coding.** Before writing any code, describe your intended approach and wait for approval. If the requirements are ambiguous or underspecified, ask clarifying questions first — do not guess and start coding.
2. **Keep tasks small.** If a task would touch more than 3 files, stop and break it into smaller, independently reviewable sub-tasks before writing any code. Propose that breakdown rather than pushing a large change through in one pass.
3. **Report the blast radius.** After writing code, list what could break as a result of the change and suggest specific tests that would cover those risks (prefer focused `chip-core` tests where possible).
4. **Reproduce bugs with a test first.** When fixing a bug, first write a failing test that reproduces it, then change code until that test passes. Don't fix a reported bug without a test that demonstrates it.
5. **Turn corrections into rules.** Whenever the user corrects you, add a new numbered rule to this section capturing the correction, so the same mistake doesn't recur.

## Build, test, lint

Requires Rust (stable, edition 2021) and `protoc` (needed by `chip-proto`'s `build.rs`; `brew install protobuf`).

```sh
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check      # CI enforces; RUSTFLAGS="-D warnings" in CI
cargo clippy --all-targets --workspace -- -D warnings
cargo test -p chip-core workflow::<name>    # run a single test (module::name from crates/chip-core/tests/workflow.rs)
cargo deny check                # advisories/licenses/bans/sources (CI `deny` job; config in deny.toml)
```

CI (`.github/workflows/ci.yml`) runs three jobs: `test` (fmt + clippy + `cargo test`), `e2e`, and `deny`. All four of fmt/clippy/test/deny must pass before a PR.

## End-to-end tests (drive real binaries)

`scripts/e2e.sh` runs the real CLI + server against a live Postgres. It needs a `psql` client (via `PG*` env) and binaries (via `CHIP_BIN`/`SERVER_BIN`, default `target/debug`). Locally:

```sh
docker run --rm -e POSTGRES_USER=chip -e POSTGRES_PASSWORD=chip -e POSTGRES_DB=chip -p 5433:5432 postgres:16
cargo build --workspace
PGHOST=localhost PGPORT=5433 PGUSER=chip PGPASSWORD=chip PGDATABASE=chip bash scripts/e2e.sh
```

It covers repo-create, push-auto-create, HTTP+SSH clone, web browse, encryption-at-rest, and two-instance statelessness.

## Running the server locally

```sh
docker compose up        # Postgres + chip-server; sets CHIP_DEV=1
```

The server refuses to boot in production mode without a non-default `CHIP_SECRET` and a 64-hex `CHIP_DATA_KEY` (`openssl rand -hex 32`, used for AES-256-GCM encryption at rest — losing it makes repos unrecoverable). Set `CHIP_DEV=1` for local runs to derive a dev key and relax the secret check. gRPC and the web UI are multiplexed on **one port** (`CHIP_BIND`, default `0.0.0.0:8080`); SSH transport is a second port (`CHIP_SSH_BIND`, default `0.0.0.0:2222`, empty disables). Full env var table is in README.md / DEPLOY.md.

## Architecture: crates and dependency direction

Cargo workspace of four crates with a strict one-way dependency direction — `chip-core` knows nothing about the network or a database:

```
chip-cli ──► chip-core        (the engine: no I/O beyond local filesystem)
    │    ──► chip-proto ◄──── chip-server ──► PostgreSQL (metadata) + object store (fs/S3)
    └── gRPC over HTTP/2 or SSH
```

- **`chip-core`** — the VCS engine, pure logic. Object model in `object.rs` (immutable Blob/Tree/Commit forming a Merkle DAG; Tree entries sorted by name for canonical hashing). Key modules: `hash` (ObjectId), `change` (ChangeId), `store/` (content-addressed backend trait), `refs`, `working_copy` (snapshot/restore, no staging), `merge` (three-way, first-class conflicts), `dag`, `diff`, `oplog`, `ops` (high-level operations the CLI calls), `evolution`.
- **`chip-cli`** — the `chip` binary. `main` (clap commands), `sync`/`remote` (gRPC client), `ssh` (SSH transport; auths via ssh-agent then `~/.ssh/id_ed25519`), `render`.
- **`chip-proto`** — gRPC service + message defs compiled from `proto/chip.proto` by `build.rs`. Regenerate by editing the `.proto` and rebuilding.
- **`chip-server`** — deployable host. `grpc` (sync), `web` (axum + askama UI, CSRF-protected forms), `db` (sqlx/Postgres; migrations in `migrations/`), `auth` (Argon2 passwords, BLAKE3-hashed revocable tokens), `crypto` (encryption at rest), `store`, `ssh` (embedded server, maps key fingerprint → account), `ratelimit`, `cache`, `config`, `validate`. Servers are stateless (state in Postgres + object store) and horizontally scalable.

## Conventions

- Match surrounding style (comment density, naming, error handling). Errors use `anyhow` in binaries and `thiserror`/`chip_core::Error` in the engine.
- Behavior changes need tests — prefer focused `chip-core` tests over e2e where possible.
- Security-sensitive areas (auth, `crypto`, `validate`, the sync protocol) warrant extra care; see SECURITY.md. Usernames/repo names are restricted to `[A-Za-z0-9_-]` to prevent path traversal into the object store.
- The `object_store` backend is selected by `CHIP_OBJECT_STORE` (`local://<path>` or `s3://<bucket>`); S3/MinIO creds come from standard `AWS_*` env vars.
- Release binaries are built by cargo-dist (`.github/workflows/release.yml`) on a `v*` tag; `deny.toml` bans wildcard deps, so internal path deps must carry versions.
