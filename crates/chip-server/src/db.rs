//! PostgreSQL access layer. Holds only relational metadata: users, tokens,
//! repositories, collaborators, and refs.

use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct TokenInfo {
    pub name: String,
    pub last_used: Option<OffsetDateTime>,
    pub expires_at: Option<OffsetDateTime>,
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: Uuid,
    pub username: String,
    pub password_hash: String,
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub id: Uuid,
    pub owner_id: Uuid,
    pub owner: String,
    pub name: String,
    pub visibility: String,
    pub description: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Read,
    Write,
}

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    pub async fn connect(url: &str, max_connections: u32) -> anyhow::Result<Db> {
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .connect(url)
            .await?;
        Ok(Db { pool })
    }

    /// Liveness probe used by `/readyz`.
    pub async fn ping(&self) -> anyhow::Result<()> {
        sqlx::query("SELECT 1").execute(&self.pool).await?;
        Ok(())
    }

    /// Run pending migrations from the workspace `migrations/` directory.
    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    // Scaling: login throttle, commit-stat cache, token cleanup ------------------

    /// True if `username` is allowed another login attempt within the window.
    pub async fn login_allowed(
        &self,
        username: &str,
        max: i32,
        window_secs: i64,
    ) -> anyhow::Result<bool> {
        let row: Option<(i32, bool)> = sqlx::query_as(
            "SELECT count, (window_start > now() - make_interval(secs => $2)) AS fresh \
             FROM login_attempts WHERE username = $1",
        )
        .bind(username)
        .bind(window_secs as f64)
        .fetch_optional(&self.pool)
        .await?;
        Ok(match row {
            Some((count, fresh)) => !(fresh && count >= max),
            None => true,
        })
    }

    /// Record a failed login, resetting the window if it has elapsed.
    pub async fn record_login_failure(
        &self,
        username: &str,
        window_secs: i64,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO login_attempts (username, count, window_start) VALUES ($1, 1, now()) \
             ON CONFLICT (username) DO UPDATE SET \
               count = CASE WHEN login_attempts.window_start > now() - make_interval(secs => $2) \
                            THEN login_attempts.count + 1 ELSE 1 END, \
               window_start = CASE WHEN login_attempts.window_start > now() - make_interval(secs => $2) \
                            THEN login_attempts.window_start ELSE now() END",
        )
        .bind(username)
        .bind(window_secs as f64)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear a user's failed-login counter (on a successful login).
    pub async fn clear_login_failures(&self, username: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM login_attempts WHERE username = $1")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Read a cached per-commit diff stat, if present.
    pub async fn get_commit_stat(
        &self,
        repo_id: Uuid,
        commit_id: &str,
    ) -> anyhow::Result<Option<(i32, i32, i32)>> {
        let row: Option<(i32, i32, i32)> = sqlx::query_as(
            "SELECT files, added, removed FROM commit_stats WHERE repo_id = $1 AND commit_id = $2",
        )
        .bind(repo_id)
        .bind(commit_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Store a per-commit diff stat (immutable; commit_id is a content hash).
    pub async fn put_commit_stat(
        &self,
        repo_id: Uuid,
        commit_id: &str,
        files: i32,
        added: i32,
        removed: i32,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO commit_stats (repo_id, commit_id, files, added, removed) \
             VALUES ($1, $2, $3, $4, $5) ON CONFLICT (repo_id, commit_id) DO NOTHING",
        )
        .bind(repo_id)
        .bind(commit_id)
        .bind(files)
        .bind(added)
        .bind(removed)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // SSH keys ------------------------------------------------------------------

    pub async fn add_ssh_key(
        &self,
        user_id: Uuid,
        name: &str,
        fingerprint: &str,
        public_key: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO ssh_keys (id, user_id, name, fingerprint, public_key) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(user_id)
        .bind(name)
        .bind(fingerprint)
        .bind(public_key)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn list_ssh_keys(&self, user_id: Uuid) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, fingerprint FROM ssh_keys WHERE user_id = $1 ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn delete_ssh_key(&self, user_id: Uuid, fingerprint: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM ssh_keys WHERE user_id = $1 AND fingerprint = $2")
            .bind(user_id)
            .bind(fingerprint)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Resolve an SSH key fingerprint to its owning user.
    pub async fn user_for_ssh_key(&self, fingerprint: &str) -> anyhow::Result<Option<User>> {
        let row: Option<(Uuid, String, String)> = sqlx::query_as(
            "SELECT u.id, u.username, u.password_hash FROM users u \
             JOIN ssh_keys k ON k.user_id = u.id WHERE k.fingerprint = $1",
        )
        .bind(fingerprint)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, username, password_hash)| User {
            id,
            username,
            password_hash,
        }))
    }

    /// Delete expired tokens (web sessions etc.). Returns the number removed.
    pub async fn delete_expired_tokens(&self) -> anyhow::Result<u64> {
        let res =
            sqlx::query("DELETE FROM tokens WHERE expires_at IS NOT NULL AND expires_at < now()")
                .execute(&self.pool)
                .await?;
        Ok(res.rows_affected())
    }

    // Users --------------------------------------------------------------------

    pub async fn create_user(
        &self,
        username: &str,
        email: &str,
        password_hash: &str,
    ) -> anyhow::Result<User> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO users (id, username, email, password_hash) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(username)
        .bind(email)
        .bind(password_hash)
        .execute(&self.pool)
        .await?;
        Ok(User {
            id,
            username: username.to_string(),
            password_hash: password_hash.to_string(),
        })
    }

    pub async fn find_user_by_username(&self, username: &str) -> anyhow::Result<Option<User>> {
        let row: Option<(Uuid, String, String)> =
            sqlx::query_as("SELECT id, username, password_hash FROM users WHERE username = $1")
                .bind(username)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(id, username, password_hash)| User {
            id,
            username,
            password_hash,
        }))
    }

    // Tokens -------------------------------------------------------------------

    pub async fn create_token(
        &self,
        user_id: Uuid,
        name: &str,
        token_hash: &str,
        expires_at: Option<OffsetDateTime>,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO tokens (id, user_id, name, token_hash, expires_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(Uuid::new_v4())
        .bind(user_id)
        .bind(name)
        .bind(token_hash)
        .bind(expires_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn user_for_token(&self, token_hash: &str) -> anyhow::Result<Option<User>> {
        // Reject expired tokens, and stamp last_used on a successful lookup.
        let row: Option<(Uuid, String, String)> = sqlx::query_as(
            "UPDATE tokens t SET last_used = now() \
             FROM users u \
             WHERE t.user_id = u.id AND t.token_hash = $1 \
               AND (t.expires_at IS NULL OR t.expires_at > now()) \
             RETURNING u.id, u.username, u.password_hash",
        )
        .bind(token_hash)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(id, username, password_hash)| User {
            id,
            username,
            password_hash,
        }))
    }

    pub async fn list_tokens(&self, user_id: Uuid) -> anyhow::Result<Vec<TokenInfo>> {
        let rows: Vec<(String, Option<OffsetDateTime>, Option<OffsetDateTime>)> = sqlx::query_as(
            "SELECT name, last_used, expires_at FROM tokens WHERE user_id = $1 \
             ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|(name, last_used, expires_at)| TokenInfo {
                name,
                last_used,
                expires_at,
            })
            .collect())
    }

    pub async fn revoke_token(&self, user_id: Uuid, name: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM tokens WHERE user_id = $1 AND name = $2")
            .bind(user_id)
            .bind(name)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // Repos --------------------------------------------------------------------

    pub async fn create_repo(
        &self,
        owner_id: Uuid,
        name: &str,
        visibility: &str,
        description: &str,
    ) -> anyhow::Result<Uuid> {
        let id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO repos (id, owner_id, name, visibility, description) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(owner_id)
        .bind(name)
        .bind(visibility)
        .bind(description)
        .execute(&self.pool)
        .await?;
        Ok(id)
    }

    pub async fn find_repo(&self, owner: &str, name: &str) -> anyhow::Result<Option<Repo>> {
        let row: Option<(Uuid, Uuid, String, String, String, String)> = sqlx::query_as(
            "SELECT r.id, r.owner_id, u.username, r.name, r.visibility, r.description \
             FROM repos r JOIN users u ON u.id = r.owner_id \
             WHERE u.username = $1 AND r.name = $2",
        )
        .bind(owner)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(id, owner_id, owner, name, visibility, description)| Repo {
                id,
                owner_id,
                owner,
                name,
                visibility,
                description,
            },
        ))
    }

    /// Repos visible to `viewer` (None = anonymous): all public repos plus any
    /// the viewer owns or collaborates on.
    pub async fn list_visible_repos(&self, viewer: Option<Uuid>) -> anyhow::Result<Vec<Repo>> {
        let rows: Vec<(Uuid, Uuid, String, String, String, String)> = sqlx::query_as(
            "SELECT DISTINCT r.id, r.owner_id, u.username, r.name, r.visibility, r.description \
             FROM repos r JOIN users u ON u.id = r.owner_id \
             LEFT JOIN collaborators c ON c.repo_id = r.id \
             WHERE r.visibility = 'public' OR r.owner_id = $1 OR c.user_id = $1 \
             ORDER BY u.username, r.name",
        )
        .bind(viewer)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(
                |(id, owner_id, owner, name, visibility, description)| Repo {
                    id,
                    owner_id,
                    owner,
                    name,
                    visibility,
                    description,
                },
            )
            .collect())
    }

    // Access control -----------------------------------------------------------

    pub async fn add_collaborator(
        &self,
        repo_id: Uuid,
        user_id: Uuid,
        role: Role,
    ) -> anyhow::Result<()> {
        let role = match role {
            Role::Read => "read",
            Role::Write => "write",
        };
        sqlx::query(
            "INSERT INTO collaborators (repo_id, user_id, role) VALUES ($1, $2, $3) \
             ON CONFLICT (repo_id, user_id) DO UPDATE SET role = EXCLUDED.role",
        )
        .bind(repo_id)
        .bind(user_id)
        .bind(role)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// The role `user` has on `repo`, accounting for ownership and visibility.
    pub async fn role_for(&self, repo: &Repo, user: Option<Uuid>) -> anyhow::Result<Option<Role>> {
        if let Some(uid) = user {
            if uid == repo.owner_id {
                return Ok(Some(Role::Write));
            }
            let row: Option<(String,)> = sqlx::query_as(
                "SELECT role FROM collaborators WHERE repo_id = $1 AND user_id = $2",
            )
            .bind(repo.id)
            .bind(uid)
            .fetch_optional(&self.pool)
            .await?;
            if let Some((role,)) = row {
                return Ok(Some(if role == "write" {
                    Role::Write
                } else {
                    Role::Read
                }));
            }
        }
        // No explicit role: read access only if the repo is public.
        if repo.visibility == "public" {
            Ok(Some(Role::Read))
        } else {
            Ok(None)
        }
    }

    // Refs ---------------------------------------------------------------------

    pub async fn list_refs(
        &self,
        repo_id: Uuid,
        is_tag: bool,
    ) -> anyhow::Result<Vec<(String, String)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT name, target FROM refs WHERE repo_id = $1 AND is_tag = $2 ORDER BY name",
        )
        .bind(repo_id)
        .bind(is_tag)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    pub async fn get_ref(
        &self,
        repo_id: Uuid,
        is_tag: bool,
        name: &str,
    ) -> anyhow::Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT target FROM refs WHERE repo_id = $1 AND is_tag = $2 AND name = $3",
        )
        .bind(repo_id)
        .bind(is_tag)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|(t,)| t))
    }

    pub async fn set_ref(
        &self,
        repo_id: Uuid,
        is_tag: bool,
        name: &str,
        target: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO refs (repo_id, name, is_tag, target) VALUES ($1, $2, $3, $4) \
             ON CONFLICT (repo_id, is_tag, name) DO UPDATE SET target = EXCLUDED.target",
        )
        .bind(repo_id)
        .bind(name)
        .bind(is_tag)
        .bind(target)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
