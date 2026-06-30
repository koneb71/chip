# chip architecture

This document explains how chip is built: its data model, the object store, the
operation model that powers `undo`, the sync protocol, and the server. It is the
companion to the user-facing [README](README.md) and the operational guides
([DEPLOY](DEPLOY.md), [SCALING](SCALING.md)).

> chip is a Git *alternative*, not a Git clone. Where a design choice diverges
> from Git, that's deliberate — the differences are called out throughout.

## Table of contents

- [Design principles](#design-principles)
- [Workspace & crates](#workspace--crates)
- [The object model](#the-object-model)
- [Identity: content hashes vs. change-ids](#identity-content-hashes-vs-change-ids)
- [The object store](#the-object-store)
- [Encryption at rest](#encryption-at-rest)
- [On-disk repository layout](#on-disk-repository-layout)
- [References & HEAD](#references--head)
- [The working copy (no staging area)](#the-working-copy-no-staging-area)
- [High-level operations](#high-level-operations)
- [Merges & first-class conflicts](#merges--first-class-conflicts)
- [The operation log & universal undo](#the-operation-log--universal-undo)
- [The sync protocol](#the-sync-protocol)
- [The server](#the-server)
- [Transports: HTTP and SSH](#transports-http-and-ssh)
- [Authentication & access control](#authentication--access-control)
- [Statelessness & scaling](#statelessness--scaling)

## Design principles

1. **Snapshots, not deltas.** Every commit captures the whole working tree.
   There is no staging area and no `add` step.
2. **Stable change identity.** A *change* keeps the same id when you amend or
   rebase it, even though its content hash changes. History stays legible across
   rewrites.
3. **Conflicts are data, not errors.** A conflicting merge produces a *conflicted
   change* you keep working with — it never aborts and never leaves you in a
   detached "mid-merge" state.
4. **Everything is undoable.** Every mutating command records the prior state, so
   `chip undo` reverses any operation without the user reasoning about reflogs.
5. **Content-addressed and pluggable.** Objects are addressed by the BLAKE3 hash
   of their content and stored behind a trait, so the same engine runs against a
   local directory, an encrypted directory, or S3 with no logic changes.

## Workspace & crates

chip is a Cargo workspace of four crates with a strict dependency direction
(`chip-core` knows nothing about the network or a database):

```
        chip-cli ───────────┐
            │               │ (gRPC over HTTP/2 or SSH)
            ▼               ▼
        chip-core      chip-proto ◄──── chip-server
       (the engine)   (wire types)     (host + web UI + auth)
                                            │
                                            ▼
                                   PostgreSQL  +  object store
                                   (metadata)     (filesystem / S3)
```

| Crate | Responsibility | Notable modules |
| ----- | -------------- | --------------- |
| `chip-core` | The version-control engine. Pure logic, no I/O beyond the local filesystem. | `object`, `hash`, `change`, `store/`, `refs`, `working_copy`, `merge`, `dag`, `diff`, `oplog`, `ops` |
| `chip-cli` | The `chip` binary: local commands plus the sync client. | `main`, `sync`, `remote`, `ssh`, `render` |
| `chip-proto` | gRPC service + message definitions, compiled from `proto/chip.proto` by `build.rs` (needs `protoc`). | generated |
| `chip-server` | The deployable host: gRPC sync, axum web UI, Postgres auth, SSH transport, encryption. | `grpc`, `web`, `db`, `auth`, `crypto`, `store`, `ssh`, `ratelimit`, `cache`, `config`, `validate` |

## The object model

Three immutable object types live in the content-addressed store
([`object.rs`](crates/chip-core/src/object.rs)):

- **Blob** — file contents (`Vec<u8>`). The unit of stored data.
- **Tree** — a directory: a list of `TreeEntry { name, kind, mode, id }`. Entries
  are **sorted by name** so the serialized form is canonical — two directories
  with the same contents hash identically regardless of insertion order. `mode`
  carries Unix permission bits (effectively just the executable bit: `0o755` vs
  `0o644`).
- **Commit** — an immutable snapshot:

  ```rust
  struct Commit {
      tree:      ObjectId,        // the root tree of this snapshot
      parents:   Vec<ObjectId>,   // 0 = root, 1 = normal, 2+ = merge
      change_id: ChangeId,        // STABLE identity (survives rewrites)
      author:    String,
      timestamp: i64,             // unix seconds
      message:   String,
      conflicts: Vec<String>,     // files left conflicted (first-class), else empty
  }
  ```

A commit points at a tree; trees point at sub-trees and blobs; commits point at
parent commits. The result is a Merkle DAG — changing any byte of any file
changes that blob's hash, which changes its tree, which changes the commit.

```
   commit (msg, change_id, author) ──► tree ──► blob  "README.md"
        │                               │
        ├─► parent commit               ├──► tree "src" ──► blob "main.rs"
        ▼                               └──► blob ".gitignore"
   parent commit ...
```

## Identity: content hashes vs. change-ids

chip deliberately separates two notions of identity:

| | `ObjectId` ([`hash.rs`](crates/chip-core/src/hash.rs)) | `ChangeId` ([`change.rs`](crates/chip-core/src/change.rs)) |
| --- | --- | --- |
| What | BLAKE3 hash of the object's canonical serialized bytes | A random 48-bit value (12 hex chars) |
| Derived from | Content | Generated once, **carried forward** |
| Changes when | Any content changes | Never (until you start a genuinely new change) |
| Length | 32 bytes / 64-hex (`short()` = 12) | 12 hex |
| Role | Deduplication, integrity, sync addressing | Human-facing change identity in `chip log` |

This is the mechanism behind "stable change-ids in action": `chip amend` and
`chip rebase` produce a **new commit** (new `ObjectId`) that **reuses the
`ChangeId`**, so the change keeps its identity even though its content hash moved.

```
$ chip commit -m wip        # change cc7ae5…   commit a1a2db…
$ chip amend  -m done       # change cc7ae5…   commit 367eed…   (same change, new commit)
```

## The object store

[`store/mod.rs`](crates/chip-core/src/store/mod.rs) implements a single canonical
encoding pipeline over a pluggable backend:

```
put:  Object ──bincode──► raw bytes ──BLAKE3──► id ──zstd(level 3)──► backend.put(id_hex, bytes)
get:  backend.get(id_hex) ──zstd⁻¹──► raw bytes ──bincode⁻¹──► Object
```

Key properties:

- **The id is the hash of the *uncompressed* canonical bytes.** Compression is a
  storage detail; it never affects identity.
- **Writes are idempotent.** The key is a content hash, so re-`put`ting an
  existing object is a cheap no-op.
- **Hash-verified ingestion.** `put_raw` (used by the sync receive path) takes the
  already-compressed wire bytes, decompresses, **re-hashes, and rejects any object
  whose content doesn't match its claimed id**, then confirms it deserializes
  before persisting. A peer cannot poison the store with mislabeled objects.
- **Zero-copy egress.** `get_raw` returns the stored compressed bytes directly, so
  the server streams objects over the wire without re-encoding.

### The backend trait

[`store/backend.rs`](crates/chip-core/src/store/backend.rs) defines the seam:

```rust
trait ObjectBackend: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Vec<u8>>>;
    fn put(&self, key: &str, bytes: &[u8]) -> Result<()>;
    fn exists(&self, key: &str) -> Result<bool> { /* default: get().is_some() */ }
}
```

- **`FilesystemBackend`** shards objects into 256 directories by the first byte of
  the key (`ab/cdef…`) to keep directories small, and writes via a sibling temp
  file + atomic rename so a reader never sees a half-written object.
- On the server, an **S3 backend** (via the `object_store` crate) implements the
  same trait — selected by `CHIP_OBJECT_STORE=s3://bucket`. The engine is
  unchanged.
- **`EncryptedBackend`** (below) is a decorator that wraps *either* of the above.

## Encryption at rest

[`crypto.rs`](crates/chip-server/src/crypto.rs) wraps any `ObjectBackend` and
AES-256-GCM-encrypts each object on the way to disk/S3:

```
stored layout per object:  [ version:1 ][ nonce:12 ][ ciphertext + GCM tag ]
                             AAD = the object's storage key (its content-hash hex)
```

- A **fresh random 96-bit nonce** per write.
- The **content-hash hex is bound in as AEAD associated data**, so a ciphertext
  cannot be silently relocated to a different address.
- Because the id is the hash of the **plaintext** (computed one layer up),
  content-addressing, the `put_raw`/`get_raw` sync path, and the gRPC wire format
  are **all unaffected** — only the bytes that land on disk change. Two servers
  sharing a key see identical addressing.
- **Tampering and wrong keys are detected** by the GCM tag (`get` returns an
  error rather than corrupt data).
- The key is `CHIP_DATA_KEY` (64 hex / 32 bytes). In `CHIP_DEV=1` mode a
  deterministic key is derived from `CHIP_SECRET` so local runs still encrypt
  without key management. **Losing `CHIP_DATA_KEY` makes encrypted repos
  unrecoverable** — see [SECURITY.md](SECURITY.md).

## On-disk repository layout

`chip init` creates a `.chip/` directory next to your working tree:

```
.chip/
├── HEAD                 # "ref: main", a detached commit hex, or empty (unborn)
├── config               # author identity
├── store/
│   ├── objects/         # the FilesystemBackend root (sharded ab/cdef…)
│   └── changes/         # reserved for change-id bookkeeping (created, not yet used)
├── refs/
│   ├── bookmarks/       # one file per bookmark → commit hex
│   └── tags/            # one file per tag → commit hex
└── oplog/
    ├── count            # highest sequence number
    └── 00000001 …       # one bincode record per operation
```

The working tree is everything *outside* `.chip` (minus `.chipignore` matches).

## References & HEAD

[`refs.rs`](crates/chip-core/src/refs.rs) manages the mutable pointers:

- **Bookmarks** are chip's named branches (`main`, a feature line, …). A bookmark
  is a file containing a commit hex.
- **Tags** are immutable named pointers at a commit.
- **`HEAD`** is one of three states: attached to a bookmark (`ref: name`),
  *detached* at a specific commit, or *unborn* (a fresh repo with no commits yet).

A *revision* argument (`rev`) accepts a bookmark, a tag, `@`/`HEAD`, a full commit
id, or an **abbreviated commit id** — the 12-char prefix `chip log` prints, which
`resolve_commit` expands by scanning commits reachable from refs.

## The working copy (no staging area)

[`working_copy.rs`](crates/chip-core/src/working_copy.rs):

- **`snapshot`** walks the whole tree (honoring `.chipignore`, always skipping
  `.chip`), stores every file as a blob, builds nested trees bottom-up, and
  returns the root tree id. There is no index/staging — `commit` snapshots
  *everything* as-is.
- **`restore`** is the inverse: it writes every file from a target tree and
  removes tracked files that aren't in that tree, leaving `.chip` untouched.
  Used by `checkout`, `undo`, `restore`, and after merges.
- **`flatten`/`build_tree`** convert between a nested tree and a flat
  `path → entry` map — the representation merges and diffs work over.

## High-level operations

[`ops.rs`](crates/chip-core/src/ops.rs) composes the lower layers into the verbs
the CLI exposes. A few are worth understanding:

| Op | What it does | Change-id behavior |
| --- | --- | --- |
| `commit` | Snapshot the tree as a new commit on `HEAD`. | Fresh change-id |
| `amend` | Re-snapshot and **replace the tip**, reusing its parents. | **Same** change-id |
| `rebase` | Replay the current first-parent line onto a new base. | **Preserves** each change-id |
| `cherry_pick` | Copy the diff a commit introduced onto `HEAD`. | Fresh change-id (it's a copy) |
| `revert` | New commit applying the **inverse** of a commit's diff. | Fresh change-id |
| `merge` | Three-way merge of another line into the current one. | Fresh merge commit |
| `restore` | Discard uncommitted changes (whole tree or one path). | — |

`cherry_pick` and `rebase` share one primitive, `apply_onto`: a three-way merge
with *base* = the picked commit's parent tree, *ours* = the target tree, *theirs*
= the picked commit's tree. `revert` is the same idea with base and theirs
swapped.

## Merges & first-class conflicts

[`merge.rs`](crates/chip-core/src/merge.rs) does a three-way merge at the tree
level (`merge_trees`), producing a `MergeResult { tree, conflicts }`. The
defining behavior: **a conflict never aborts.** Conflicting regions are written
into the file with markers, and the affected paths are recorded in the commit's
`conflicts` list. You get a normal (if conflicted) change you can keep building
on; `chip resolve` re-snapshots and clears the paths whose markers you removed.

`dag.rs` supplies the graph queries merges rely on: `merge_base` (lowest common
ancestor), `is_ancestor` (fast-forward detection), `history`, and
`reachable_objects` (an **iterative** traversal — no recursion, so deep histories
don't blow the stack).

## The operation log & universal undo

[`oplog.rs`](crates/chip-core/src/oplog.rs) is what makes *every* command
reversible. Before a mutating command runs, chip **captures the current ref state**
(`HEAD` + all bookmark targets) into a `RepoState`. After the command, it appends
an `Op { seq, timestamp, description, before }` record under `.chip/oplog/`.

`chip undo` reads the latest record, restores its `before` ref state, restores the
working tree to match the now-current `HEAD`, and drops the record. Because the
snapshot is of the *pointers* (and objects are immutable and content-addressed),
undo is cheap and works uniformly for commit, merge, rebase, checkout — anything.

```
chip commit ─► capture refs ─► do work ─► append Op{seq:n, before}
chip undo   ─► read Op{seq:n} ─► restore before refs + working tree ─► drop record
```

## The sync protocol

The CLI and server speak one gRPC service, `ChipSync`
([`proto/chip.proto`](proto/chip.proto)):

| RPC | Direction | Purpose |
| --- | --- | --- |
| `Register` / `Login` | client → server | Obtain an auth token. |
| `ListRefs` | client → server | The repo's bookmarks + tags (clone/pull/push negotiation). |
| `FetchObjects` | server → client (stream) | Stream every object reachable from `want` but not `have`. |
| `Push` | client → server (stream) | Upload objects, then apply ref updates. |

Transfers are a classic **want/have negotiation** computed with
`dag::reachable_objects`:

- **clone** — `want` = every server ref target, `have` = ∅; download everything,
  then write the refs locally.
- **pull** — `want` = server refs, `have` = local commits; download the missing
  closure, then integrate by the chosen strategy (fast-forward, `--rebase`, or
  `--merge`).
- **push** — send `reachable(local refs) − reachable(server refs)` as a stream of
  `ObjectChunk`s, then a `RefUpdates` message. The server stores each object via
  the **hash-verifying** `put_raw`, checks every ref target actually exists in the
  store, and applies updates **fast-forward-only unless `force`** is set.

Objects cross the wire as their **already-compressed stored bytes** (`get_raw` →
`ObjectChunk { id, data }` → `put_raw`): no re-encoding, and integrity is
re-verified on receipt.

```
PUSH stream:  [ PushHeader{owner,repo} ][ ObjectChunk ]…[ ObjectChunk ][ RefUpdates ]
                                                          server: put_raw (verify hash)
                                                          then apply refs (ff unless force)
```

## The server

[`chip-server`](crates/chip-server) hosts repositories for many users. gRPC and
the web UI are **multiplexed on a single port** — requests whose path starts with
`/chip.ChipSync/` are routed to the tonic service, everything else to the axum
web app — so a deployment needs just one mapped domain.

Two storage tiers, split by what each is good at:

- **PostgreSQL** holds only relational metadata: `users`, `tokens`, `repos`,
  `collaborators`, `refs`, `login_attempts`, `commit_stats`, `ssh_keys`. Refs live
  here (not in the object store) so they're transactional and shared across
  replicas.
- **The object store** (filesystem or S3, wrapped in `EncryptedBackend`) holds the
  immutable content-addressed objects.

The web UI ([`web.rs`](crates/chip-server/src/web.rs)) is server-rendered HTML
(no build step, no JS framework): account pages, a repo overview, a file browser
(tree + blob views), change/diff views, and token/SSH-key settings. Per-commit
diff stats are memoized in the `commit_stats` table (a commit id is a content
hash, so its stats are immutable and safe to cache forever).

## Transports: HTTP and SSH

The **same gRPC service** is reachable two ways:

- **HTTP/2** — tonic over h2, optionally with in-process native TLS (rustls via
  `axum-server`, with the aws-lc-rs provider; ALPN advertises `h2` + `http/1.1`).
  The CLI uses system CA roots for `https://` endpoints and a stored **bearer
  token** for auth.
- **SSH** — an embedded `russh` server ([`ssh.rs`](crates/chip-server/src/ssh.rs)).
  It authenticates by **public-key fingerprint** (mapping a key's SHA-256
  fingerprint to a user via the `ssh_keys` table), then **tunnels the very same
  gRPC service over the SSH channel**: the authenticated user is injected as a
  request extension, so every RPC and all access-control checks work unchanged.
  The CLI ([`chip-cli/src/ssh.rs`](crates/chip-cli/src/ssh.rs)) authenticates via
  ssh-agent first (Unix), falling back to `~/.ssh/id_ed25519`/`id_ecdsa`/`id_rsa`,
  and pins the server host key on first use (`known_hosts` TOFU).

Because SSH carries the identical protocol, `clone`/`push`/`pull` behave
identically regardless of transport.

## Authentication & access control

- **Passwords** are hashed with **Argon2** ([`auth.rs`](crates/chip-server/src/auth.rs));
  the minimum length is `auth::MIN_PASSWORD_LEN` (8).
- **API tokens** are random, shown once, and stored **only as BLAKE3 hashes**.
  They are revocable and can carry an expiry.
- **CSRF** tokens on every state-changing web form are derived as
  `BLAKE3(secret ∥ "csrf" ∥ session)` — unforgeable without the server secret and
  underivable without the `HttpOnly` session cookie.
- **Login rate limiting** is enforced **cluster-wide via Postgres**
  (`login_attempts`), so it holds across replicas
  ([`ratelimit.rs`](crates/chip-server/src/ratelimit.rs)).
- **Response hardening**: a tight `Content-Security-Policy`, plus
  `X-Frame-Options: DENY`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, and `SameSite=Strict` session cookies.
- **Repository access control**: repos are public or private; access is the union
  of ownership and `collaborators` rows (`read`/`write`). `role_for` gates every
  read and write. Pushes additionally require `write` and are fast-forward-only
  unless forced.
- **Input validation**: usernames and repo names are restricted to
  `[A-Za-z0-9_-]{1,64}` ([`validate.rs`](crates/chip-server/src/validate.rs)),
  which structurally prevents path traversal into the object store.

## Statelessness & scaling

The server keeps **no authoritative state in process** — the source of truth is
Postgres plus the object store. In-process structures are pure caches
(`dashmap`-backed token cache, per-repo store factory, commit-stat cache), each
safe to lose or duplicate. That makes the server **horizontally scalable**: run
N identical replicas behind a load balancer, all pointed at the same database and
object store (S3 for true multi-writer). Refs are transactional in Postgres, so
concurrent pushes from different replicas stay consistent.

See [SCALING.md](SCALING.md) for the path to ~10k users/repos/RPS and the
infrastructure choices (connection pooling, read replicas, CDN-fronted object
storage).
