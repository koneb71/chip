-- SSH public keys for key-based auth over the SSH transport.
CREATE TABLE IF NOT EXISTS ssh_keys (
    id          UUID PRIMARY KEY,
    user_id     UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    fingerprint TEXT UNIQUE NOT NULL, -- SHA256:... of the public key
    public_key  TEXT NOT NULL,        -- the openssh authorized_keys line
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_ssh_keys_user ON ssh_keys (user_id);
