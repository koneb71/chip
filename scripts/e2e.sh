#!/usr/bin/env bash
# End-to-end test for chip: exercises the real binaries against a live Postgres.
# Covers CLI repo-create, push-auto-create, HTTP + SSH clone, the web UI
# (security headers, repo browse, description display), encryption at rest, and
# two-instance statelessness.
#
# Configuration (env, with defaults):
#   PGHOST/PGPORT/PGUSER/PGPASSWORD/PGDATABASE  Postgres connection (psql client)
#   CHIP_BIN     path to the `chip` CLI binary       (default target/debug/chip)
#   SERVER_BIN   path to the `chip-server` binary     (default target/debug/chip-server)
#   HTTP_PORT    server HTTP port                     (default 8090)
#   SSH_PORT     server SSH port                      (default 2229)
#
# Locally:   PGPORT=5433 bash scripts/e2e.sh   (against a Docker postgres on 5433)
# In CI:     a postgres:16 service on localhost:5432 (see .github/workflows/ci.yml)
set -uo pipefail

ROOT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT_DIR"

export PGHOST="${PGHOST:-localhost}"
export PGPORT="${PGPORT:-5432}"
export PGUSER="${PGUSER:-chip}"
export PGPASSWORD="${PGPASSWORD:-chip}"
export PGDATABASE="${PGDATABASE:-chip}"
CHIP_BIN="${CHIP_BIN:-$ROOT_DIR/target/debug/chip}"
SERVER_BIN="${SERVER_BIN:-$ROOT_DIR/target/debug/chip-server}"
HTTP_PORT="${HTTP_PORT:-8090}"
SSH_PORT="${SSH_PORT:-2229}"

PASS=0; FAIL=0
ok()  { echo "  ✅ $1"; PASS=$((PASS+1)); }
bad() { echo "  ❌ $1"; FAIL=$((FAIL+1)); }
psql_q() { psql -v ON_ERROR_STOP=1 -qtA -c "$1"; }

WORKDIR="$(mktemp -d "${TMPDIR:-/tmp}/chip-e2e.XXXXXX")"
export HOME_CFG="$WORKDIR/home"; mkdir -p "$HOME_CFG/.ssh"
OBJ="$WORKDIR/repos"; mkdir -p "$OBJ"
MARKER="PLAINTEXT_MARKER_8f3a2b_secret"
DESC="A friendly project description"

cleanup() { kill "${SRV1:-0}" "${SRV2:-0}" 2>/dev/null; }
trap cleanup EXIT

echo "== 0. wait for Postgres =="
for i in $(seq 1 30); do
  if psql -qtA -c 'SELECT 1' >/dev/null 2>&1; then ok "postgres reachable (${i}s)"; break; fi
  sleep 1
  [ "$i" = 30 ] && { bad "postgres unreachable at $PGHOST:$PGPORT"; exit 1; }
done
# Fresh schema each run.
psql_q "DROP SCHEMA public CASCADE; CREATE SCHEMA public;" >/dev/null

echo "== 1. start server (HTTP :$HTTP_PORT, SSH :$SSH_PORT) =="
CHIP_BIND="0.0.0.0:$HTTP_PORT" CHIP_SSH_BIND="0.0.0.0:$SSH_PORT" \
CHIP_SSH_HOST_KEY="$WORKDIR/ssh_host_key" \
DATABASE_URL="postgres://$PGUSER:$PGPASSWORD@$PGHOST:$PGPORT/$PGDATABASE" \
CHIP_OBJECT_STORE="local://$OBJ" \
CHIP_SECRET="e2e-secret-not-the-default" CHIP_DEV=1 \
CHIP_BASE_URL="http://localhost:$HTTP_PORT" \
  "$SERVER_BIN" >"$WORKDIR/server1.log" 2>&1 &
SRV1=$!
for i in $(seq 1 30); do
  curl -fsS "http://localhost:$HTTP_PORT/healthz" >/dev/null 2>&1 && { ok "server healthy (${i}s)"; break; }
  sleep 1
  [ "$i" = 30 ] && { bad "server never came up"; cat "$WORKDIR/server1.log"; exit 1; }
done

echo "== 2. register (password >= 8) =="
HOME="$HOME_CFG" "$CHIP_BIN" register "http://localhost:$HTTP_PORT" -u alice -e a@x.com -p password123 \
  >"$WORKDIR/reg.log" 2>&1 && ok "registered alice" || { bad "register failed"; cat "$WORKDIR/reg.log"; }
HOME="$HOME_CFG" "$CHIP_BIN" register "http://localhost:$HTTP_PORT" -u shorty -e s@x.com -p short1 \
  >/dev/null 2>&1 && bad "short password accepted (policy broken)" || ok "short password rejected (>=8)"

echo "== 3. create repo via the CLI (chip repo create) =="
HOME="$HOME_CFG" "$CHIP_BIN" repo create "http://localhost:$HTTP_PORT/alice/proj" --public --description "$DESC" \
  >"$WORKDIR/create.log" 2>&1 && ok "chip repo create succeeded" || { bad "repo create failed"; cat "$WORKDIR/create.log"; }
# Duplicate create must be rejected.
HOME="$HOME_CFG" "$CHIP_BIN" repo create "http://localhost:$HTTP_PORT/alice/proj" \
  >/dev/null 2>&1 && bad "duplicate repo create accepted" || ok "duplicate repo create rejected"
# Creating under someone else's namespace must be rejected.
HOME="$HOME_CFG" "$CHIP_BIN" repo create "http://localhost:$HTTP_PORT/bob/x" \
  >/dev/null 2>&1 && bad "cross-namespace create accepted" || ok "cross-namespace create rejected"

echo "== 4. commit + push over HTTP =="
WORK="$WORKDIR/work"; mkdir -p "$WORK"
( cd "$WORK"
  HOME="$HOME_CFG" "$CHIP_BIN" init >/dev/null 2>&1
  printf 'hello %s\n' "$MARKER" > note.txt
  mkdir -p sub; printf 'nested %s\n' "$MARKER" > sub/deep.txt
  HOME="$HOME_CFG" "$CHIP_BIN" commit -m "initial" >/dev/null 2>&1
  HOME="$HOME_CFG" "$CHIP_BIN" remote add origin "http://localhost:$HTTP_PORT/alice/proj" >/dev/null 2>&1
  HOME="$HOME_CFG" "$CHIP_BIN" push origin >"$WORKDIR/push.log" 2>&1 ) \
  && ok "pushed over HTTP" || { bad "push failed"; cat "$WORKDIR/push.log"; }

echo "== 5. push auto-create (second repo, no prior create) =="
WORK2="$WORKDIR/work2"; mkdir -p "$WORK2"
( cd "$WORK2"
  HOME="$HOME_CFG" "$CHIP_BIN" init >/dev/null 2>&1
  printf 'auto %s\n' "$MARKER" > auto.txt
  HOME="$HOME_CFG" "$CHIP_BIN" commit -m "auto" >/dev/null 2>&1
  HOME="$HOME_CFG" "$CHIP_BIN" remote add origin "http://localhost:$HTTP_PORT/alice/autocreated" >/dev/null 2>&1
  HOME="$HOME_CFG" "$CHIP_BIN" push origin >"$WORKDIR/push2.log" 2>&1 ) \
  && ok "push auto-created alice/autocreated" || { bad "push auto-create failed"; cat "$WORKDIR/push2.log"; }
[ "$(psql_q "SELECT count(*) FROM repos WHERE name='autocreated'")" = "1" ] \
  && ok "auto-created repo row exists (private)" || bad "auto-created repo row missing"

echo "== 6. clone over HTTP =="
HOME="$HOME_CFG" "$CHIP_BIN" clone "http://localhost:$HTTP_PORT/alice/proj" "$WORKDIR/clone-http" \
  >"$WORKDIR/clone-http.log" 2>&1 && ok "cloned over HTTP" || { bad "http clone failed"; cat "$WORKDIR/clone-http.log"; }
grep -q "$MARKER" "$WORKDIR/clone-http/note.txt" 2>/dev/null && ok "http clone content matches" || bad "http clone mismatch"
grep -q "$MARKER" "$WORKDIR/clone-http/sub/deep.txt" 2>/dev/null && ok "nested file matches" || bad "nested file mismatch"

echo "== 7. web: security headers =="
HDRS="$(curl -fsSI "http://localhost:$HTTP_PORT/" 2>/dev/null)"
echo "$HDRS" | grep -qi "content-security-policy:" && ok "CSP present" || bad "CSP missing"
echo "$HDRS" | grep -qi "x-frame-options: *DENY" && ok "X-Frame-Options: DENY" || bad "X-Frame-Options missing"
echo "$HDRS" | grep -qi "x-content-type-options: *nosniff" && ok "nosniff present" || bad "nosniff missing"

echo "== 8. web: browse + description display =="
curl -fsS "http://localhost:$HTTP_PORT/alice/proj/tree/main" 2>/dev/null | grep -q "note.txt" \
  && ok "tree lists note.txt" || bad "tree view broken"
curl -fsS "http://localhost:$HTTP_PORT/alice/proj/blob/main/note.txt" 2>/dev/null | grep -q "$MARKER" \
  && ok "blob shows content" || bad "blob view broken"
curl -fsS "http://localhost:$HTTP_PORT/alice/proj" 2>/dev/null | grep -q "$DESC" \
  && ok "description shows on overview" || bad "description missing on overview"

echo "== 9. SSH transport =="
ssh-keygen -t ed25519 -N '' -C alice@e2e -f "$HOME_CFG/.ssh/id_ed25519" >/dev/null 2>&1
FP="$(ssh-keygen -lf "$HOME_CFG/.ssh/id_ed25519.pub" | awk '{print $2}')"
PUB="$(cat "$HOME_CFG/.ssh/id_ed25519.pub")"
psql_q "INSERT INTO ssh_keys (id, user_id, name, fingerprint, public_key)
        SELECT gen_random_uuid(), id, 'laptop', '$FP', '$PUB' FROM users WHERE username='alice';" >/dev/null \
  && ok "ssh key registered" || bad "ssh key insert failed"
HOME="$HOME_CFG" "$CHIP_BIN" clone "ssh://chip@localhost:$SSH_PORT/alice/proj" "$WORKDIR/clone-ssh" \
  >"$WORKDIR/clone-ssh.log" 2>&1 && ok "cloned over SSH" || { bad "ssh clone failed"; cat "$WORKDIR/clone-ssh.log"; }
grep -q "$MARKER" "$WORKDIR/clone-ssh/note.txt" 2>/dev/null && ok "ssh clone content matches" || bad "ssh clone mismatch"

echo "== 10. encryption at rest (plaintext absent on disk) =="
if grep -rqa "$MARKER" "$OBJ" 2>/dev/null; then
  bad "PLAINTEXT FOUND in object store — encryption not applied!"
else
  ok "marker absent in object files ($(find "$OBJ" -type f | wc -l | tr -d ' ') objects, ciphertext at rest)"
fi

echo "== 11. two-instance statelessness (2nd server, same DB+objects) =="
PORT2=$((HTTP_PORT+5))
CHIP_BIND="0.0.0.0:$PORT2" CHIP_SSH_BIND= \
DATABASE_URL="postgres://$PGUSER:$PGPASSWORD@$PGHOST:$PGPORT/$PGDATABASE" \
CHIP_OBJECT_STORE="local://$OBJ" \
CHIP_SECRET="e2e-secret-not-the-default" CHIP_DEV=1 \
CHIP_BASE_URL="http://localhost:$PORT2" \
  "$SERVER_BIN" >"$WORKDIR/server2.log" 2>&1 &
SRV2=$!
for i in $(seq 1 30); do curl -fsS "http://localhost:$PORT2/healthz" >/dev/null 2>&1 && break; sleep 1; done
HOME="$HOME_CFG" "$CHIP_BIN" clone "http://localhost:$PORT2/alice/proj" "$WORKDIR/clone-2nd" \
  >"$WORKDIR/clone-2nd.log" 2>&1 && ok "cloned from 2nd instance" || { bad "2nd-instance clone failed"; cat "$WORKDIR/clone-2nd.log"; }
grep -q "$MARKER" "$WORKDIR/clone-2nd/note.txt" 2>/dev/null && ok "2nd-instance content matches" || bad "2nd-instance mismatch"

echo
echo "==================== RESULT: $PASS passed, $FAIL failed ===================="
[ "$FAIL" -eq 0 ]
