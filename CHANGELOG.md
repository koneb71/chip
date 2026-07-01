# Changelog

All notable changes to chip are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from 1.0 onward.

## [Unreleased]

### Added
- **Render cache**: a bounded in-process LRU memoizes the expensive, immutable web
  renders — syntax-highlighted blobs, rendered READMEs, diff HTML, and history
  walks — keyed by content hash (no invalidation needed). No new infrastructure.
- **Git import**: `chip import git <path> [dir]` imports a local Git repository's
  history into a new chip repo — Git blobs/trees/commits map to chip objects with
  author, message, timestamp, parents, branches (→ bookmarks) and tags preserved
  (fresh change-ids; content re-hashed with BLAKE3). Pure-Rust (`gix`), no C deps.
- **Stacked changes & evolution** (CLI): `chip stack` visualizes the chain of
  changes above the trunk; `chip evolution [rev]` shows a change's commit versions
  over time (recorded automatically on `amend`/`rebase`, since the change-id stays
  stable while the commit hash moves).
- **Change requests**: propose merging one bookmark into another, review the
  combined diff, comment, approve / request changes, and **merge from the web UI**
  (server-side three-way merge — no working copy needed; conflicts stay
  first-class and are surfaced instead of force-merged).
- **Web code browser polish**: server-side **syntax highlighting** for source
  files, sanitized **README rendering** on the repository overview, and a
  per-file **History** view (commits that changed a file).
- `chip repo create <url>/owner/repo [--public] [--description …]` — create a
  server-side repository from the CLI (new `CreateRepo` gRPC RPC).
- **Push-to-create**: the first `chip push` to a repository under your own
  namespace creates it automatically (private), so you no longer need the web UI
  to get started.
- Optional repository **description**, set when creating a repo (CLI or web) and
  shown on the repository overview and the index listing.
- A friendlier `/new` web form: description field, name-rule hint, a live
  `host/owner/name` URL preview, and Private/Public visibility cards.
- An **empty-repository quick start** panel on the overview with the exact
  `chip remote add` + `chip push` commands.
- Continuous integration: an end-to-end job (live Postgres + server, exercising
  HTTP/SSH clone, web browse, encryption at rest, and statelessness) and a
  `cargo-deny` dependency-audit job.

## [0.0.1]

Initial release: a changeset-oriented version control system (a Git
*alternative*) in Rust, with a deployable multi-user server.

### Version control engine
- Whole-tree snapshot commits — **no staging area**.
- **Stable change-ids** that survive `amend`/`rebase`, distinct from BLAKE3
  content hashes.
- **First-class conflicts**: merges never abort; conflicts are recorded on the
  change and resolved with a normal commit.
- **Universal `chip undo`** backed by an operation log.
- Content-addressed object store (BLAKE3 + zstd) behind a pluggable backend
  (local filesystem or S3-compatible object storage).
- Commands: `init`, `commit`, `log`, `status`, `diff`, `bookmark`, `checkout`,
  `tag`, `show`, `merge`, `rebase`, `cherry-pick`, `revert`, `restore`, `amend`,
  `resolve`, `undo`, `op log`.

### Server
- gRPC sync + server-rendered web UI multiplexed on one port.
- PostgreSQL-backed accounts, API tokens, repositories, collaborators, and refs.
- HTTP (bearer token, optional TLS) and **SSH** (public-key) transports.
- Web file browser (tree + blob), change/diff views, token and SSH-key settings.
- Public/private repositories with read/write collaborators.
- Horizontal scalability: the server is stateless over Postgres + the object
  store.

### Security
- AES-256-GCM **encryption at rest** for object data (`CHIP_DATA_KEY`).
- Argon2 password hashing; BLAKE3-hashed, revocable API tokens.
- CSRF protection, response security headers (CSP, frame/sniff/referrer),
  `SameSite=Strict` session cookies.
- Cluster-wide login rate limiting; path-traversal-safe identifier validation.

### Distribution
- Cross-platform release binaries + installers via cargo-dist (macOS arm64/x64,
  Linux x64/arm64 musl, Windows x64).

[Unreleased]: https://github.com/koneb71/chip/compare/v0.0.1...HEAD
[0.0.1]: https://github.com/koneb71/chip/releases/tag/v0.0.1
