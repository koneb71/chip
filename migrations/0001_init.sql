-- Accounts, repositories, access control, and per-repo refs.
-- Object data itself lives in the object store, not Postgres.

CREATE TABLE users (
    id            UUID PRIMARY KEY,
    username      TEXT UNIQUE NOT NULL,
    email         TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    created_at    TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE tokens (
    id         UUID PRIMARY KEY,
    user_id    UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    token_hash TEXT UNIQUE NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_used  TIMESTAMPTZ
);

CREATE TABLE repos (
    id         UUID PRIMARY KEY,
    owner_id   UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT NOT NULL,
    visibility TEXT NOT NULL DEFAULT 'private', -- 'private' | 'public'
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (owner_id, name)
);

CREATE TABLE collaborators (
    repo_id UUID NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    user_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    role    TEXT NOT NULL, -- 'read' | 'write'
    PRIMARY KEY (repo_id, user_id)
);

CREATE TABLE refs (
    repo_id UUID NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    name    TEXT NOT NULL,
    is_tag  BOOLEAN NOT NULL DEFAULT false,
    target  TEXT NOT NULL, -- commit id, hex
    PRIMARY KEY (repo_id, is_tag, name)
);
