# Scaling chip

How chip scales from a single box to ~10k users / repos / requests-per-second.
The application changes that make this possible already landed (see "What's
already done"); the rest is infrastructure you turn on as load grows.

## Architecture in one line

Stateless server replicas behind a load balancer, all sharing **Postgres**
(metadata) and an **object store** (encrypted blobs). No replica holds
source-of-truth state, so you scale the app tier by adding identical instances.

```
            ┌─────────────┐
   clients ─┤ load balancer│─┬─► chip-server #1 ─┐
   (CLI/web)└─────────────┘ ├─► chip-server #2 ─┼─► Postgres (primary + read replica)
                            └─► chip-server #N ─┘   object store (S3 / MinIO)  ──► CDN
                                   │
                                   └─► Redis (cache + cluster-wide rate limit)  [higher scale]
```

## What's already done (in-app)

These shipped in the server and make a single instance fast and N instances
viable:

- **Stateless server.** The only per-replica state is caches (correctness-neutral).
  Login rate limiting is in Postgres (`login_attempts`), so it holds across all
  replicas.
- **Auth is no longer per-request DB I/O.** A 60s token cache collapses the
  per-request SELECT + `last_used` UPDATE to ~one DB round-trip per token per
  minute.
- **Object stores are cached** and the S3 client + its runtime are built once at
  startup (not per request).
- **`FetchObjects` streams lazily** through a bounded channel — server memory stays
  flat regardless of repo size — and the reachability walk is iterative (no
  stack-overflow on deep history).
- **Browse is cheap.** Per-commit diff stats are cached in `commit_stats` (keyed by
  the immutable content hash), so repo pages don't recompute diffs.
- **DB indexes** on the hot queries; configurable pool (`CHIP_DB_MAX_CONNECTIONS`);
  hourly cleanup of expired tokens; `/readyz` + graceful shutdown for rolling deploys.

## The infra path (turn on as you grow)

### 1. Horizontal app tier (first lever)
Run multiple `chip-server` replicas behind a load balancer. In Dokploy, scale the
service replica count; on Kubernetes, a `Deployment` + `HorizontalPodAutoscaler`
on CPU/RPS. Gate traffic on `/readyz`. Because the server is stateless, this is
pure capacity — no session affinity needed. gRPC (HTTP/2) load-balances per-request
through an L7 proxy (Traefik/Envoy/NGINX).

### 2. Postgres
- Put **PgBouncer** (transaction pooling) in front so thousands of client
  connections fan into a small server-side pool.
- Add a **read replica** and route read-mostly endpoints (repo browse, repo list,
  ref lists) to it; keep writes (push, register, token issue) on the primary.
- The indexes from migration `0003` cover the current hot queries; watch
  `pg_stat_statements` and add more as access patterns evolve.
- If `tokens` / `login_attempts` ever get hot, partition or move them to Redis (§4).

### 3. Object storage + CDN
- Use **S3 / MinIO** (`CHIP_OBJECT_STORE=s3://…`) as the shared blob tier — already
  supported. This is the real long-run scaling lever for data volume.
- Objects are **content-addressed and immutable**, so public reads can sit behind a
  **CDN** with effectively infinite cache TTL. (Encrypted-at-rest objects are
  decrypted by the server; for CDN-served public repos, consider a per-repo
  public-read path that serves plaintext blobs, or client-side decryption.)

### 4. Redis (cluster-wide caches at higher scale)
Swap the per-replica in-process caches for a shared Redis when one DB or cache
node becomes the limit:
- **Token/session cache** (replaces the per-replica `TokenCache`).
- **Cluster-wide rate limiting** (replaces the Postgres `login_attempts` table with
  an atomic Redis counter — cheaper at high QPS).
- **Ref/list and rendered-page caching** with short TTLs.

### 5. Throughput optimizations
- **Pack-file transfer:** batch many small objects into a few framed blobs to cut
  per-object overhead on large clones.
- **Object dedup across forks:** today each repo has its own key prefix; a shared
  content-addressed pool would dedup identical blobs across forks (big storage win).
- **gRPC compression** on the object stream for slow links.

### 6. Observability + load testing
- **Metrics:** export Prometheus metrics (request rate/latency/errors, DB pool
  utilization, cache hit rate, object-store latency). **Tracing:** OpenTelemetry
  spans across web → DB → object store.
- **Load test before you need to:** `k6` against the web/HTTP endpoints and `ghz`
  against `FetchObjects`/`Push`. Add a replica and confirm throughput scales ~linearly;
  whatever saturates first (usually Postgres connections or object-store latency) is
  your next target.

## Capacity intuition (order of magnitude)

| Tier | ~10k total users | ~10k concurrent / 10k RPS |
|------|------------------|----------------------------|
| App  | 1–2 replicas | autoscale 4–N replicas behind LB |
| Postgres | single primary + indexes | primary + PgBouncer + read replica |
| Cache | in-app (built-in) | Redis |
| Objects | filesystem volume or S3 | S3 + CDN |

The app tier scales out trivially; Postgres and object-store latency are the
limits to watch — instrument them and add the replica/CDN levers above when the
load test says so.
