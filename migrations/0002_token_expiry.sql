-- Optional expiry for API tokens, so leaked/stale tokens can age out.
ALTER TABLE tokens ADD COLUMN expires_at TIMESTAMPTZ NULL;
