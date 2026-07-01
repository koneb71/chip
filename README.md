# chip

[![CI](https://github.com/koneb71/chip/actions/workflows/ci.yml/badge.svg)](https://github.com/koneb71/chip/actions/workflows/ci.yml)
[![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)
[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org)

A changeset-oriented version control system — a Git *alternative*, built from
scratch in Rust — with a deployable, multi-user server.

chip is **not** a Git clone. Its model is deliberately different:

- **No staging area.** There is no `add`. The whole working tree is snapshotted
  on `commit`.
- **Stable change IDs.** Every change has a random `change-id` that persists
  across rewrites, distinct from its content hash (a BLAKE3 `commit` id).
- **First-class conflicts.** A conflicting merge never aborts — it produces a
  *conflicted change* you keep working with, then resolve with a normal commit.
- **Universal undo.** Every mutating command records an operation; `chip undo`
  reverses the last one.
- **Content-addressed store** (BLAKE3 + zstd) behind a pluggable backend:
  local filesystem today, S3-compatible object storage (MinIO/AWS) by config.

## Workspace layout

| Crate          | What it is                                                        |
|----------------|-------------------------------------------------------------------|
| `chip-core`    | The VCS engine: object store, snapshot, dag, three-way merge, oplog |
| `chip-cli`     | The `chip` binary — local VCS + gRPC remote client                |
| `chip-proto`   | gRPC service definitions (`proto/chip.proto`) + generated stubs   |
| `chip-server`  | The server — gRPC sync + axum web UI + Postgres auth + object store |

For a deep dive into the data model, object store, encryption, sync protocol, and
server internals, see **[ARCHITECTURE.md](ARCHITECTURE.md)**.

## Install the CLI

Prebuilt `chip` binaries are published for macOS (arm64/x64), Linux (x64/arm64,
static musl), and Windows (x64) on each tagged release — no Rust or protoc needed.

```sh
# macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koneb71/chip/releases/latest/download/chip-cli-installer.sh | sh
```

```powershell
# Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/koneb71/chip/releases/latest/download/chip-cli-installer.ps1 | iex"
```

Or build from source (needs Rust + `protoc`): `cargo install --path crates/chip-cli`.

> Windows notes: keys live in `%USERPROFILE%\.ssh`, config in `%APPDATA%\chip`.
> The SSH transport uses key files (ssh-agent integration is Unix-only); the HTTP
> transport works everywhere. Releases are produced by cargo-dist
> (`.github/workflows/release.yml`) on a `v*` tag.

## Quick start

From zero to a synced repository (assumes a server at `http://localhost:8080` —
see [Server](#server) to run one):

```sh
# 1. make a local repo and a first change (no staging step)
mkdir demo && cd demo
chip init
echo "# Demo" > README.md
chip commit -m "first commit"

# 2. create an account on the server
chip register http://localhost:8080 -u alice -e alice@example.com

# 3. create the remote repo and push to it
chip repo create http://localhost:8080/alice/demo --description "My first chip repo"
chip remote add origin http://localhost:8080/alice/demo
chip push origin
# (shortcut: skip `repo create` — the first push to your own namespace
#  auto-creates a private repo)

# 4. clone it somewhere else
cd .. && chip clone http://localhost:8080/alice/demo demo-clone
```

Browse the result at `http://localhost:8080/alice/demo`.

<!-- TODO: record an asciinema cast of the above and embed it here, e.g.
     [![asciicast](https://asciinema.org/a/XXXXXX.svg)](https://asciinema.org/a/XXXXXX) -->

## CLI

```sh
chip init                     # create a repository
chip commit -m "msg"          # snapshot the whole tree as a new change
chip log                      # history, organized by change id
chip status                   # working-tree changes since the last commit
chip diff                     # unified diff of those changes
chip bookmark <name>          # create/move a bookmark (named branch)
chip checkout <name|commit>   # switch and update the working tree
chip checkout -b <name>       # create a bookmark at HEAD and switch to it
chip tag <name>               # tag the current commit
chip show [rev]               # change metadata + diff (rev defaults to @)
chip merge <name|commit>      # three-way merge (conflicts stay first-class)
chip rebase <name|commit>     # replay the whole branch onto a new base (keeps change-ids)
chip cherry-pick <rev>        # copy one commit's change onto the current change
chip revert <rev>             # new commit that undoes a previous commit
chip restore [path]           # discard uncommitted changes (whole tree, or a file)
chip amend [-m msg]           # rewrite the current change, keeping its change-id
chip resolve                  # clear resolved conflict markers (keeps change-id)
chip undo                     # reverse the last operation (any command)
chip op log                   # list recorded operations
chip stack                    # show the stack of changes above the trunk
chip evolution [rev]          # how a change evolved across amend/rebase (its versions)
chip import git <path> [dir]  # import a local Git repo's history into a new chip repo

# Remotes — HTTP (token) or SSH (key)
chip register <url> -u alice -e a@x.com   # HTTP; password prompted if -p omitted
chip login <url> -u alice
chip repo create <url>/alice/proj [--public] [--description "…"]  # create a server repo
chip clone http://host:8080/alice/proj    # HTTP transport (bearer token)
chip clone ssh://chip@host:2222/alice/proj # SSH transport (your key)
chip remote add origin <url>/alice/proj
chip push origin [--force]    # first push to your own namespace auto-creates the repo
chip pull origin              # fast-forward; warns (never clobbers) on divergence
chip pull origin --rebase     # on divergence, rebase local changes onto the remote
chip pull origin --merge      # on divergence, create a merge commit
```

### SSH transport

Add your **public key** under "SSH keys" in the web UI, then use an `ssh://`
(or scp-style `chip@host:owner/repo`) remote. The CLI authenticates via
**ssh-agent** first (so passphrase-protected keys work), falling back to
`~/.ssh/id_ed25519` (or `id_ecdsa`/`id_rsa`) — no token needed; the server maps
your key's fingerprint to your account. The server host key is pinned on first
use in `~/.config/chip/known_hosts` and verified on every connection. SSH tunnels
the same gRPC sync protocol, so clone/push/pull behave identically to HTTP.

Revisions (`rev`) accept a bookmark, a tag, `@`/`HEAD`, or a commit id —
abbreviated ids (the 12-char prefix `chip log` prints) resolve against the
commits visible from refs.

There is no staging step: edit files, then `chip commit`.

### Stable change-ids in action

`chip amend` and `chip rebase` rewrite a change **in place** — the content
(commit) hash changes but the `change-id` stays the same, so the change keeps its
identity in `chip log`:

```
$ chip commit -m wip        # change cc7ae5…  commit a1a2db…
$ chip amend -m done        # change cc7ae5…  commit 367eed…   (same change, new commit)
```

## Server

The server hosts repositories, authenticates the CLI with bearer tokens, serves
a web UI to browse changes/diffs (plus a public `/docs` page), and enforces
per-repo access control (public/private + read/write collaborators). gRPC and the web UI are
multiplexed on **one port**, so a deployment needs a single mapped domain.

Safeguards: passwords are Argon2-hashed; CLI tokens are stored only as BLAKE3
hashes (revocable); pushes are **fast-forward by default** (a non-fast-forward
update is rejected unless `--force`); and state-changing web forms carry a CSRF
token derived from the session and `CHIP_SECRET`.

### Configuration (environment)

| Variable            | Default                     | Purpose                              |
|---------------------|-----------------------------|--------------------------------------|
| `CHIP_BIND`         | `0.0.0.0:8080`              | Listen address                       |
| `DATABASE_URL`      | *(required)*                | Postgres connection string           |
| `CHIP_OBJECT_STORE` | `local:///data/repos`       | `local://<path>` or `s3://<bucket>`  |
| `CHIP_SECRET`       | *(required)*                | Secret for CSRF tokens; refuses default unless `CHIP_DEV=1` |
| `CHIP_DATA_KEY`     | *(required)*                | 64-hex (32-byte) AES key for encryption at rest |
| `CHIP_BASE_URL`     | `http://localhost:8080`     | Public URL shown in the UI           |
| `CHIP_COOKIE_SECURE`| auto (true if base URL https) | Force the `Secure` flag on session cookies |
| `CHIP_TLS_CERT` / `CHIP_TLS_KEY` | *(none)*       | PEM paths to serve native TLS (h2 + http/1.1) |
| `CHIP_DB_MAX_CONNECTIONS` | `25`               | Postgres pool size per server instance |
| `CHIP_SSH_BIND`     | `0.0.0.0:2222`              | SSH transport bind (empty disables SSH) |
| `CHIP_SSH_HOST_KEY` | `chip_ssh_host_key`         | Path to the SSH host key (generated on first run) |
| `CHIP_DEV`          | *(unset)*                   | Relax prod requirements for local dev |

For `s3://`, credentials/region/endpoint come from the standard `AWS_*` env vars
(which also cover MinIO via `AWS_ENDPOINT` + path-style).

### Security

- **Encryption at rest.** Every stored object is AES-256-GCM encrypted (per-object
  nonce, the object's hash bound in as AEAD associated data). The object id is the
  hash of the *plaintext*, so content-addressing and sync are unaffected.
  **⚠ Losing `CHIP_DATA_KEY` makes stored repositories unrecoverable** — back it up
  out-of-band. Generate one with `openssl rand -hex 32`.
- **Transport.** Set `CHIP_TLS_CERT`/`CHIP_TLS_KEY` to terminate TLS in-process,
  or terminate at a proxy (see Dokploy note). The CLI uses system CA roots for
  `https://` endpoints.
- **Auth.** Argon2 passwords; CLI tokens stored only as BLAKE3 hashes, revocable,
  optionally expiring; failed logins are rate-limited per username.
- **Other.** Usernames/repo names are restricted to `[A-Za-z0-9_-]` (no path
  traversal into the object store); web mutations carry CSRF tokens; pushes are
  fast-forward unless `--force`.

### Run locally

```sh
docker compose up --build          # Postgres + server, objects on a volume
# open http://localhost:8080, register, create a repo, then `chip push`

docker compose --profile s3 up      # also start MinIO to exercise the s3:// backend
```

Postgres holds only relational metadata (users, tokens, repos, collaborators,
refs). Repository object data lives in the object store.

## Deploying

- **EC2 (single instance, Docker Compose + native TLS):** see **[DEPLOY.md](DEPLOY.md)**
  — uses `docker-compose.prod.yml` + `.env.production.example`.
- **Scaling out** (replicas, RDS, S3, CDN): see **[SCALING.md](SCALING.md)**.

### Deploying to Dokploy

1. Provision a **Postgres** database in Dokploy (or keep the compose `postgres`
   service).
2. Create an application from this repository (Dokploy builds the `Dockerfile`).
3. Set env: `DATABASE_URL`, a strong `CHIP_SECRET`, a `CHIP_DATA_KEY`
   (`openssl rand -hex 32`, **backed up** — losing it loses the data),
   `CHIP_BASE_URL`, and `CHIP_OBJECT_STORE` (`local:///data/repos` on a persistent
   volume, or an `s3://` bucket with `AWS_*` creds — no code change).
4. Map a domain to the app's port `8080`. Health checks hit `/healthz`. The proxy
   should terminate TLS (the cookie `Secure` flag turns on automatically when
   `CHIP_BASE_URL` is `https://`).

> Note: gRPC behind a reverse proxy needs HTTP/2 cleartext (h2c) to the backend.
> Traefik (Dokploy's proxy) supports this; if your proxy makes single-port gRPC
> awkward, run the web UI and gRPC on separate ports/domains.

## Scaling

The server is stateless (source-of-truth lives in Postgres + the object store), so
it scales out as identical replicas behind a load balancer. See **[SCALING.md](SCALING.md)**
for the architecture and the infra path to ~10k users / repos / RPS.

## Testing

```sh
cargo test            # object store, dag/merge-base, three-way merge, oplog, sync
```

## Contributing

Contributions are welcome! See **[CONTRIBUTING.md](CONTRIBUTING.md)** for build
prerequisites, the dev workflow, and the lint/format/test gate CI enforces.
Security issues should be reported privately — see **[SECURITY.md](SECURITY.md)**.

## License

Licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT)), or
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option. Unless you explicitly state otherwise, any contribution you
intentionally submit for inclusion in this project shall be dual-licensed as
above, without any additional terms or conditions.
