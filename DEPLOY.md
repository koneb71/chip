# Deploying chip to EC2

A single-instance Docker Compose deployment with native TLS. For scaling beyond
one box (replicas, RDS, S3, CDN) see [SCALING.md](SCALING.md).

## 0. Why this shape

chip serves gRPC (HTTP/2) and the web UI on **one port** via ALPN, so the server
**terminates TLS itself** — no reverse proxy juggling h2c. Clients connect
directly over `https://` (web + gRPC) and `:2222` (SSH).

## 1. Launch the instance

- **AMI:** Ubuntu 22.04 or 24.04 LTS (default login user is `ubuntu`).
- **Type:** `c7i-flex.large` (2 vCPU / 4 GB, x86_64) is a good pick. Serving is
  comfortable; the only catch is the **release build** can pressure 4 GB of RAM —
  add swap (step 4.5) or build via ECR and `docker compose pull`.
- **Storage:** 30 GB+ gp3 (holds Postgres + repo objects).
- **Elastic IP:** allocate one and associate it, so the address is stable.
- **DNS:** point an A record (`chip.example.com`) at the Elastic IP.

### No domain yet?

You can run without one:

- **IP + HTTP, SSH for repos (simplest).** Skip TLS: in `.env` leave
  `CHIP_TLS_CERT`/`CHIP_TLS_KEY` blank, set `CHIP_BASE_URL=http://<ELASTIC_IP>:8080`,
  and in `docker-compose.prod.yml` change the server port mapping to
  `"8080:8080"` and drop the `/etc/letsencrypt` volume. The web UI is then plaintext
  (fine for testing) — do real clone/push/pull over the encrypted **SSH
  transport** (`ssh://chip@<ELASTIC_IP>:2222/...`), which needs no domain or TLS.
- **Free HTTPS via nip.io.** The hostname `<dashed-ip>.nip.io` resolves to your
  IP and Let's Encrypt **will** issue for it, so you get real TLS with no domain
  purchase. Use it everywhere a domain appears below, e.g.
  `sudo certbot certonly --standalone -d 13-37-1-2.nip.io` and
  `CHIP_BASE_URL=https://13-37-1-2.nip.io`. (A raw IP or the `ec2-…amazonaws.com`
  name cannot get a trusted cert.)

### Security group (inbound)

| Port | Source        | Purpose                          |
|------|---------------|----------------------------------|
| 443  | 0.0.0.0/0     | HTTPS — web UI + gRPC            |
| 2222 | 0.0.0.0/0     | chip SSH transport              |
| 80   | 0.0.0.0/0     | **temporary**, for certbot only |
| 22   | your IP only  | admin SSH (the instance's sshd) |

> chip's SSH transport is on **2222**, separate from the instance's own admin
> SSH on **22** — no conflict.

## 2. Install Docker (Ubuntu)

```sh
sudo apt update && sudo apt install -y git
# Docker Engine + Compose plugin via Docker's convenience script:
curl -fsSL https://get.docker.com | sudo sh
sudo usermod -aG docker ubuntu     # then log out and back in
docker compose version             # confirm the compose plugin is present
```

## 3. Get the code and a TLS certificate

```sh
git clone <your-chip-repo> chip && cd chip

# One-time cert via certbot standalone (needs port 80 open + DNS resolving):
sudo apt install -y certbot
sudo certbot certonly --standalone -d chip.example.com
# Certs land in /etc/letsencrypt/live/chip.example.com/{fullchain,privkey}.pem
```

> **certbot standalone needs inbound TCP 80** reachable from the internet. A
> `Timeout during connect (likely firewall problem)` means your **EC2 Security
> Group** is missing an HTTP/80 rule (a *timeout*, vs "connection refused", is the
> tell-tale of a closed Security Group) — add `HTTP 80 from 0.0.0.0/0` and retry.
>
> **Cert-related crash loop?** If `docker compose logs server` repeats
> `failed to read from file '…fullchain.pem': No such file or directory`, check:
> (1) the cert actually exists — `sudo ls -lL /etc/letsencrypt/live/<domain>/`;
> (2) `CHIP_TLS_CERT`/`CHIP_TLS_KEY` in `.env` use the **full**
> `/etc/letsencrypt/live/<domain>/…` paths and the domain matches the directory
> name. The compose file mounts all of `/etc/letsencrypt` so certbot's symlinks
> resolve inside the container.

## 4. Configure secrets

```sh
cp .env.production.example .env
# Generate strong values:
echo "CHIP_SECRET=$(openssl rand -hex 32)"
echo "CHIP_DATA_KEY=$(openssl rand -hex 32)"
# Edit .env: paste those, set POSTGRES_PASSWORD, DATABASE_URL, CHIP_BASE_URL,
# and point CHIP_TLS_CERT/CHIP_TLS_KEY at your domain under
# /etc/letsencrypt/live/<your-domain>/ (replace chip.example.com everywhere).
nano .env
```

> ⚠️ **Back up `CHIP_DATA_KEY` offline** (e.g. AWS Secrets Manager). Lose it and
> every stored repository is unrecoverable — it's the at-rest encryption key.

## 4.5 Add swap (4 GB instances)

The release build can exceed 4 GB of RAM. Add temporary swap so it can't OOM:

```sh
sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile
sudo mkswap /swapfile && sudo swapon /swapfile
# (optional) keep it: echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

Alternatively, cap build parallelism: prefix the build with
`CARGO_BUILD_JOBS=2`, or build the image elsewhere and push to ECR.

## 5. Launch

```sh
docker compose -f docker-compose.prod.yml up -d --build
docker compose -f docker-compose.prod.yml logs -f server   # watch it boot
curl -k https://localhost/healthz                          # -> ok
```

Open `https://chip.example.com`, register the first account, add an SSH key under
**SSH keys**, create a repo, then from your laptop:

```sh
chip clone https://chip.example.com/<owner>/<repo>          # HTTP + token
chip clone ssh://chip@chip.example.com:2222/<owner>/<repo>  # SSH + key
```

## 6. Operate

- **Update:** `git pull && docker compose -f docker-compose.prod.yml up -d --build`.
- **Backups:** the `CHIP_DATA_KEY` (offline), the `pgdata` volume
  (`pg_dump` on a schedule), and the `repodata` volume (or the S3 bucket).
- **Cert renewal:** `sudo certbot renew && docker compose -f docker-compose.prod.yml restart server`
  (cron it). Certbot needs port 80 reachable during renewal.

## 7. AWS-native upgrades (optional, recommended for real load)

- **Postgres → Amazon RDS:** create an RDS Postgres, point `DATABASE_URL` at it,
  and remove the `postgres` service from `docker-compose.prod.yml`. Managed
  backups + failover.
- **Objects → S3:** set `CHIP_OBJECT_STORE=s3://your-bucket` and attach an
  **EC2 instance IAM role** granting `s3:GetObject/PutObject/ListBucket` on the
  bucket (no static keys needed). Durable, and lets you scale to multiple
  instances behind a load balancer (the server is stateless — see SCALING.md).
- **TLS via ACM + NLB:** if you prefer AWS-managed certs, front the instance with
  a **Network Load Balancer** (L4 TCP passthrough on 443/2222) and terminate TLS
  on the server as above, or terminate ACM TLS at the NLB and run the server
  plaintext behind it (set `CHIP_COOKIE_SECURE=1`).

## 8. Backup & restore

chip's durable state lives in **three** places, and all three are needed to
restore a working instance:

| What | Where | Why it matters |
| ---- | ----- | -------------- |
| **Secrets** — `CHIP_DATA_KEY`, `CHIP_SECRET` | your `.env` / secrets manager | Object data is AES-256-GCM encrypted; **without the exact `CHIP_DATA_KEY` every repository is permanently unreadable.** |
| **Metadata** | PostgreSQL (`pgdata` volume or RDS) | Accounts, tokens, repos, collaborators, refs, SSH keys. |
| **Object data** | object store (`repodata` volume or S3 bucket) | The content-addressed, encrypted commit/tree/blob objects. |

> ⚠ **Back up `CHIP_DATA_KEY` first, offline, and separately from the data.** It
> is not stored in the database and cannot be recovered. Treat it like a root
> password: a secrets manager (AWS Secrets Manager, 1Password, `pass`), not the
> server's disk. The same applies to `CHIP_SECRET` (sessions/CSRF).

### Taking a backup

```sh
# 1. Secrets — copy these somewhere safe ONCE; they rarely change.
grep -E 'CHIP_DATA_KEY|CHIP_SECRET' .env     # store the values in your secrets manager

# 2. Postgres metadata — schedule this (cron / systemd timer).
docker compose -f docker-compose.prod.yml exec -T postgres \
  pg_dump -U chip chip | gzip > "chip-db-$(date +%F).sql.gz"

# 3a. Object data on a local volume — snapshot the directory.
docker run --rm -v chip_repodata:/data -v "$PWD":/backup alpine \
  tar czf "/backup/chip-objects-$(date +%F).tar.gz" -C /data .
# 3b. Object data on S3 — enable bucket Versioning + a lifecycle policy, or sync:
#     aws s3 sync s3://your-bucket s3://your-backup-bucket
```

Objects are immutable and content-addressed, so object backups are safe to take
incrementally and at a different cadence than the database. For a consistent
point-in-time set, back up Postgres **after** the object store (refs only point at
objects that already exist).

### Restoring

1. Provision a fresh host / Postgres / object store.
2. **Set the original `CHIP_DATA_KEY` and `CHIP_SECRET`** in `.env` — the *exact*
   values from the old instance.
3. Restore Postgres:
   ```sh
   gunzip -c chip-db-YYYY-MM-DD.sql.gz | \
     docker compose -f docker-compose.prod.yml exec -T postgres psql -U chip chip
   ```
4. Restore the object store (untar into the `repodata` volume, or point
   `CHIP_OBJECT_STORE` at the restored S3 bucket).
5. Start the server. Schema migrations run automatically on boot.
6. **Verify:** log in to the web UI, open a repository, confirm the file browser
   renders a blob (proves objects decrypt with the restored key), and do a
   `chip clone` over HTTP and SSH.

### Rotating the data key

There is no built-in re-encryption tool yet, so **keep `CHIP_DATA_KEY` stable**.
Rotating it means decrypting every object with the old key and re-encrypting with
the new one; until that tooling exists, plan key rotation as a maintenance
migration rather than a routine operation.
