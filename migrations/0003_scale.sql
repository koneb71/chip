-- Scaling: indexes for hot queries, cluster-wide login throttling, and a
-- read-through cache of per-commit diff stats.

-- Indexes covering the hot read paths.
CREATE INDEX IF NOT EXISTS idx_collaborators_user ON collaborators (user_id);
CREATE INDEX IF NOT EXISTS idx_repos_visibility ON repos (visibility);
CREATE INDEX IF NOT EXISTS idx_tokens_expires_at ON tokens (expires_at);
CREATE INDEX IF NOT EXISTS idx_refs_repo ON refs (repo_id);

-- Cluster-wide login rate limiting (shared across replicas).
CREATE TABLE IF NOT EXISTS login_attempts (
    username     TEXT PRIMARY KEY,
    count        INT NOT NULL DEFAULT 0,
    window_start TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Immutable per-commit diff stats (commit_id is a content hash), so this is a
-- forever-cache shared by all replicas.
CREATE TABLE IF NOT EXISTS commit_stats (
    repo_id   UUID NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    commit_id TEXT NOT NULL,
    files     INT NOT NULL,
    added     INT NOT NULL,
    removed   INT NOT NULL,
    PRIMARY KEY (repo_id, commit_id)
);
