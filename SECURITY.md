# Security Policy

## Supported versions

chip is pre-1.0. Security fixes land on `main` and in the latest tagged release.
Until 1.0, only the most recent release is supported.

| Version | Supported |
| ------- | --------- |
| latest `main` / newest tag | ✅ |
| older tags | ❌ |

## Reporting a vulnerability

**Please do not open a public issue for security problems.**

Report privately via GitHub's [private security advisories](https://docs.github.com/en/code-security/security-advisories/guidance-on-reporting-and-writing-information-about-vulnerabilities/privately-reporting-a-security-vulnerability)
(the **"Report a vulnerability"** button under the repository's **Security** tab),
or by email to the maintainers at `<SECURITY_CONTACT_EMAIL>` *(set this before
publishing)*.

Please include enough detail to reproduce: affected version/commit, a
description of the impact, and steps or a proof of concept. We aim to acknowledge
reports within 72 hours and to ship a fix or mitigation as quickly as the
severity warrants. We'll credit reporters who want it once a fix is released.

## Security model & operator responsibilities

chip is designed to be self-hosted. A few properties are worth understanding when
you deploy it:

- **Encryption at rest.** When `CHIP_DATA_KEY` is set, object content is encrypted
  with AES-256-GCM before it touches disk or object storage. **This key is not
  recoverable.** If you lose it, every encrypted repository is permanently
  unreadable — back it up in a secrets manager, not in the repo or the same disk.
- **Secrets.** `CHIP_SECRET` (session/CSRF derivation) and `CHIP_DATA_KEY` must be
  long, random, and kept out of version control. Rotate them only with a migration
  plan — rotating `CHIP_DATA_KEY` requires re-encrypting existing objects.
- **TLS.** Terminate TLS (native or at a reverse proxy) for any internet-facing
  deployment. Set `cookie_secure` so session cookies are only sent over HTTPS.
- **Passwords & tokens.** Passwords are hashed with Argon2; API tokens are stored
  only as BLAKE3 hashes. Login attempts are rate-limited cluster-wide via the
  database.
- **Abuse protection at the edge.** chip does not implement IP-based registration
  throttling or CAPTCHA itself (it has no reliable client-IP source behind a
  proxy). For public instances, enforce per-IP rate limiting and bot mitigation at
  your reverse proxy / load balancer, and consider disabling open registration.
- **Input validation.** Usernames and repository names are restricted to
  `[A-Za-z0-9_-]{1,64}`, which prevents path traversal into the object store.

## Known advisories

`cargo audit` / `cargo deny` report a few advisories with no available upstream
fix. They are tracked in [`deny.toml`](deny.toml) with rationale:

- **RUSTSEC-2023-0071 (`rsa`, medium).** A timing sidechannel in RSA *private-key*
  operations, reachable via the SSH transport's RSA client-key support. The chip
  **server holds no RSA private key** (its host key is ed25519) and only *verifies*
  client signatures — a public-key operation — so the private-key oracle this
  advisory describes is not reachable server-side. There is no fixed release. If
  you don't need RSA client keys, you can drop RSA support entirely by removing the
  `rsa` feature from the `russh` dependency.
- **RUSTSEC-2025-0141 (`bincode` 1.x, unmaintained).** Stable, and it defines the
  on-disk object encoding; a 2.x migration changes the format and is tracked
  separately, not as a security fix.
- **RUSTSEC-2025-0134 (`rustls-pemfile`, unmaintained).** Transitive; superseded by
  `rustls-pki-types` and drops out on the next TLS-stack bump.

## Built-in hardening

- Argon2 password hashing, BLAKE3-hashed API tokens
- CSRF tokens bound to session + server secret on all state-changing web forms
- Session cookies: `HttpOnly`, `SameSite=Strict`, `Secure` (when configured)
- Response security headers: `Content-Security-Policy`, `X-Content-Type-Options`,
  `X-Frame-Options`, `Referrer-Policy`
- Cluster-wide login rate limiting
- AES-256-GCM encryption at rest (optional, via `CHIP_DATA_KEY`)
