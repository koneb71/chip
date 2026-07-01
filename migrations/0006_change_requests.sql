-- Change requests: propose merging one bookmark into another, with review.

CREATE TABLE change_requests (
    id         UUID PRIMARY KEY,
    repo_id    UUID NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
    number     INT  NOT NULL,                      -- per-repo sequential (#1, #2…)
    title      TEXT NOT NULL,
    body       TEXT NOT NULL DEFAULT '',
    author_id  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    source_ref TEXT NOT NULL,                       -- bookmark to merge from
    target_ref TEXT NOT NULL,                       -- bookmark to merge into
    state      TEXT NOT NULL DEFAULT 'open',        -- open | merged | closed
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (repo_id, number)
);
CREATE INDEX idx_cr_repo ON change_requests (repo_id, state);

CREATE TABLE cr_comments (
    id         UUID PRIMARY KEY,
    cr_id      UUID NOT NULL REFERENCES change_requests(id) ON DELETE CASCADE,
    author_id  UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    body       TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX idx_crc_cr ON cr_comments (cr_id, created_at);

CREATE TABLE cr_reviews (
    cr_id       UUID NOT NULL REFERENCES change_requests(id) ON DELETE CASCADE,
    reviewer_id UUID NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    verdict     TEXT NOT NULL,                      -- approve | request_changes
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (cr_id, reviewer_id)
);
