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
  `"8080:8080"` and drop the `/certs` volume. The web UI is then plaintext
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

## 4. Configure secrets

```sh
cp .env.production.example .env
# Generate strong values:
echo "CHIP_SECRET=$(openssl rand -hex 32)"
echo "CHIP_DATA_KEY=$(openssl rand -hex 32)"
# Edit .env: paste those, set POSTGRES_PASSWORD, DATABASE_URL, CHIP_BASE_URL,
# and CERT_DOMAIN=chip.example.com
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
