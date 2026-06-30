-- Optional human-readable description for a repository, shown in the web UI.
-- Additive and safe on existing rows (defaults to empty).
ALTER TABLE repos ADD COLUMN description TEXT NOT NULL DEFAULT '';
