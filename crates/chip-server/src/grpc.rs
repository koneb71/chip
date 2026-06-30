//! The `ChipSync` gRPC service: auth + repository synchronization.

use std::pin::Pin;
use std::str::FromStr;

use chip_core::hash::ObjectId;
use chip_core::store::ObjectStore;
use futures::Stream;
use tonic::{Request, Response, Status, Streaming};

use chip_proto::chip_sync_server::ChipSync;
use chip_proto::{
    push_request, AuthResponse, CreateRepoRequest, CreateRepoResponse, FetchRequest,
    ListRefsRequest, ListRefsResponse, LoginRequest, ObjectChunk, PushRequest, PushResponse, Ref,
    RegisterRequest,
};

use crate::auth;
use crate::db::{Db, Repo, Role, User};
use crate::store::StoreFactory;

pub struct ChipService {
    pub db: Db,
    pub stores: StoreFactory,
    pub limiter: std::sync::Arc<crate::ratelimit::RateLimiter>,
    pub tokens: std::sync::Arc<crate::cache::TokenCache>,
}

type ObjectStream = Pin<Box<dyn Stream<Item = Result<ObjectChunk, Status>> + Send>>;

/// Identity injected by the SSH transport (key-authenticated user), carried as a
/// request extension in lieu of a bearer token.
#[derive(Clone)]
pub struct SshIdentity(pub User);

impl ChipService {
    /// Resolve the requesting user: an SSH-injected identity if present,
    /// otherwise the bearer token in request metadata.
    async fn current_user<T>(&self, req: &Request<T>) -> Result<Option<User>, Status> {
        if let Some(id) = req.extensions().get::<SshIdentity>() {
            return Ok(Some(id.0.clone()));
        }
        let Some(value) = req.metadata().get("authorization") else {
            return Ok(None);
        };
        let value = value
            .to_str()
            .map_err(|_| Status::unauthenticated("bad token"))?;
        let token = value.strip_prefix("Bearer ").unwrap_or(value);
        let hash = auth::hash_token(token);
        self.tokens.user_for_token(&hash).await.map_err(internal)
    }

    /// Load a repo and the requesting user's role, enforcing minimum access.
    async fn authorize<T>(
        &self,
        req: &Request<T>,
        owner: &str,
        name: &str,
        need: Role,
    ) -> Result<(Repo, Option<User>), Status> {
        let user = self.current_user(req).await?;
        let repo = self
            .db
            .find_repo(owner, name)
            .await
            .map_err(internal)?
            .ok_or_else(|| Status::not_found("no such repository"))?;
        let role = self
            .db
            .role_for(&repo, user.as_ref().map(|u| u.id))
            .await
            .map_err(internal)?;
        let ok = match (need, role) {
            // Read is satisfied by any role (public repos grant anonymous read).
            (Role::Read, Some(_)) => true,
            (Role::Write, Some(Role::Write)) => true,
            _ => false,
        };
        if !ok {
            return Err(Status::permission_denied(
                "insufficient access to repository",
            ));
        }
        // Only write requires an authenticated user; read may be anonymous.
        if need == Role::Write && user.is_none() {
            return Err(Status::unauthenticated("authentication required"));
        }
        Ok((repo, user))
    }
}

#[tonic::async_trait]
impl ChipSync for ChipService {
    async fn register(
        &self,
        req: Request<RegisterRequest>,
    ) -> Result<Response<AuthResponse>, Status> {
        let r = req.into_inner();
        if !crate::validate::valid_name(&r.username) {
            return Err(Status::invalid_argument(
                "username must be 1-64 chars of letters, digits, '-' or '_'",
            ));
        }
        if r.password.len() < auth::MIN_PASSWORD_LEN {
            return Err(Status::invalid_argument(
                "password must be at least 8 characters",
            ));
        }
        if self
            .db
            .find_user_by_username(&r.username)
            .await
            .map_err(internal)?
            .is_some()
        {
            return Err(Status::already_exists("username taken"));
        }
        let hash = auth::hash_password(&r.password).map_err(internal)?;
        let user = self
            .db
            .create_user(&r.username, &r.email, &hash)
            .await
            .map_err(internal)?;
        let token = issue_token(&self.db, &user, "cli").await?;
        Ok(Response::new(AuthResponse {
            token,
            username: user.username,
        }))
    }

    async fn login(&self, req: Request<LoginRequest>) -> Result<Response<AuthResponse>, Status> {
        let r = req.into_inner();
        if !self.limiter.allowed(&r.username).await {
            return Err(Status::resource_exhausted(
                "too many failed login attempts; try again later",
            ));
        }
        let user = self
            .db
            .find_user_by_username(&r.username)
            .await
            .map_err(internal)?
            .filter(|u| auth::verify_password(&r.password, &u.password_hash));
        let user = match user {
            Some(u) => {
                self.limiter.record_success(&r.username).await;
                u
            }
            None => {
                self.limiter.record_failure(&r.username).await;
                return Err(Status::unauthenticated("invalid credentials"));
            }
        };
        let token = issue_token(&self.db, &user, "cli").await?;
        Ok(Response::new(AuthResponse {
            token,
            username: user.username,
        }))
    }

    async fn create_repo(
        &self,
        req: Request<CreateRepoRequest>,
    ) -> Result<Response<CreateRepoResponse>, Status> {
        let user = self
            .current_user(&req)
            .await?
            .ok_or_else(|| Status::unauthenticated("authentication required"))?;
        let r = req.into_inner();
        if r.owner != user.username {
            return Err(Status::permission_denied(
                "you can only create repositories under your own account",
            ));
        }
        if !crate::validate::valid_name(&r.repo) {
            return Err(Status::invalid_argument(
                "repository name must be 1-64 chars of letters, digits, '-' or '_'",
            ));
        }
        if self
            .db
            .find_repo(&r.owner, &r.repo)
            .await
            .map_err(internal)?
            .is_some()
        {
            return Err(Status::already_exists("repository already exists"));
        }
        let visibility = if r.public { "public" } else { "private" };
        self.db
            .create_repo(user.id, &r.repo, visibility, r.description.trim())
            .await
            .map_err(internal)?;
        Ok(Response::new(CreateRepoResponse {
            created: true,
            message: format!("created {}/{}", r.owner, r.repo),
        }))
    }

    async fn list_refs(
        &self,
        req: Request<ListRefsRequest>,
    ) -> Result<Response<ListRefsResponse>, Status> {
        let r = req.get_ref().clone();
        let (repo, _) = self.authorize(&req, &r.owner, &r.repo, Role::Read).await?;
        let bookmarks = self
            .db
            .list_refs(repo.id, false)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|(name, target)| Ref { name, target })
            .collect();
        let tags = self
            .db
            .list_refs(repo.id, true)
            .await
            .map_err(internal)?
            .into_iter()
            .map(|(name, target)| Ref { name, target })
            .collect();
        Ok(Response::new(ListRefsResponse { bookmarks, tags }))
    }

    type FetchObjectsStream = ObjectStream;

    async fn fetch_objects(
        &self,
        req: Request<FetchRequest>,
    ) -> Result<Response<Self::FetchObjectsStream>, Status> {
        let r = req.get_ref().clone();
        let (_repo, _) = self.authorize(&req, &r.owner, &r.repo, Role::Read).await?;
        let store = self
            .stores
            .repo_store(&r.owner, &r.repo)
            .map_err(internal)?;

        // Compute the (small) id set to send on a blocking thread.
        let store2 = store.clone();
        let (want, have) = (r.want.clone(), r.have.clone());
        let ids = tokio::task::spawn_blocking(move || compute_send_ids(&store2, &want, &have))
            .await
            .map_err(internal)?
            .map_err(internal)?;

        // Stream objects one at a time through a bounded channel — a blocking
        // task reads each object and the bounded send applies backpressure, so
        // memory stays O(channel capacity), not O(repo size).
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<ObjectChunk, Status>>(16);
        tokio::task::spawn_blocking(move || {
            for id in ids {
                let chunk = match store.get_raw(&id) {
                    Ok(Some(bytes)) => Ok(ObjectChunk {
                        id: id.to_hex(),
                        data: bytes,
                    }),
                    Ok(None) => continue,
                    Err(e) => Err(internal(e)),
                };
                let is_err = chunk.is_err();
                if tx.blocking_send(chunk).is_err() || is_err {
                    break;
                }
            }
        });

        let stream = futures::stream::unfold(rx, |mut rx| async move {
            rx.recv().await.map(|item| (item, rx))
        });
        Ok(Response::new(Box::pin(stream)))
    }

    async fn push(
        &self,
        req: Request<Streaming<PushRequest>>,
    ) -> Result<Response<PushResponse>, Status> {
        // Extract identity up front (the streaming request is not Send, so we must
        // not hold it across an await), then consume the stream.
        let ssh_user = req.extensions().get::<SshIdentity>().map(|s| s.0.clone());
        let token = req
            .metadata()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.strip_prefix("Bearer ").unwrap_or(v).to_string());
        let mut stream = req.into_inner();
        let user = match (ssh_user, &token) {
            (Some(u), _) => Some(u),
            (None, Some(t)) => self
                .tokens
                .user_for_token(&auth::hash_token(t))
                .await
                .map_err(internal)?,
            (None, None) => None,
        };

        let mut header: Option<(String, String)> = None;
        let mut pending_objects: Vec<ObjectChunk> = Vec::new();
        let mut updates: Vec<push_request::Body> = Vec::new();

        while let Some(msg) = stream.message().await? {
            match msg.body {
                Some(push_request::Body::Header(h)) => header = Some((h.owner, h.repo)),
                Some(push_request::Body::Object(o)) => pending_objects.push(o),
                Some(b @ push_request::Body::Updates(_)) => updates.push(b),
                None => {}
            }
        }

        let (owner, name) =
            header.ok_or_else(|| Status::invalid_argument("missing push header"))?;
        let repo = match self.db.find_repo(&owner, &name).await.map_err(internal)? {
            Some(repo) => repo,
            // Auto-create on first push: only under your own namespace, only with a
            // valid name, and always private. Anything else stays "not found".
            None => {
                let owns = user.as_ref().map(|u| u.username.as_str()) == Some(owner.as_str());
                if owns && crate::validate::valid_name(&name) {
                    let uid = user.as_ref().expect("owns implies authenticated").id;
                    self.db
                        .create_repo(uid, &name, "private", "")
                        .await
                        .map_err(internal)?;
                    self.db
                        .find_repo(&owner, &name)
                        .await
                        .map_err(internal)?
                        .ok_or_else(|| internal("repository vanished after creation"))?
                } else {
                    return Err(Status::not_found("no such repository"));
                }
            }
        };
        let role = self
            .db
            .role_for(&repo, user.as_ref().map(|u| u.id))
            .await
            .map_err(internal)?;
        if role != Some(Role::Write) {
            return Err(Status::permission_denied("write access required"));
        }

        let store = self.stores.repo_store(&owner, &name).map_err(internal)?;

        // Store objects (with hash verification) on a blocking thread.
        let store2 = store.clone();
        tokio::task::spawn_blocking(move || -> Result<(), Status> {
            for chunk in pending_objects {
                let id = ObjectId::from_str(&chunk.id)
                    .map_err(|_| Status::invalid_argument("bad object id"))?;
                store2.put_raw(&id, &chunk.data).map_err(internal)?;
            }
            Ok(())
        })
        .await
        .map_err(internal)??;

        // Apply ref updates, verifying each target exists in the store.
        for body in updates {
            if let push_request::Body::Updates(ups) = body {
                for u in ups.updates {
                    let id = ObjectId::from_str(&u.new_target)
                        .map_err(|_| Status::invalid_argument("bad ref target"))?;
                    let store3 = store.clone();
                    let exists = tokio::task::spawn_blocking(move || store3.contains(&id))
                        .await
                        .map_err(internal)?
                        .map_err(internal)?;
                    if !exists {
                        return Err(Status::failed_precondition(format!(
                            "ref {} points at missing object {}",
                            u.name,
                            id.short()
                        )));
                    }

                    // Enforce fast-forward for bookmarks unless forced: the new
                    // target must be a descendant of the current one.
                    if !u.is_tag && !u.force {
                        if let Some(current) = self
                            .db
                            .get_ref(repo.id, false, &u.name)
                            .await
                            .map_err(internal)?
                        {
                            if let Ok(old) = ObjectId::from_str(&current) {
                                let store4 = store.clone();
                                let ff = tokio::task::spawn_blocking(move || {
                                    chip_core::dag::is_ancestor(&store4, old, id)
                                })
                                .await
                                .map_err(internal)?
                                .unwrap_or(false);
                                if !ff {
                                    return Err(Status::failed_precondition(format!(
                                        "non-fast-forward update to bookmark '{}' (use --force to override)",
                                        u.name
                                    )));
                                }
                            }
                        }
                    }

                    self.db
                        .set_ref(repo.id, u.is_tag, &u.name, &u.new_target)
                        .await
                        .map_err(internal)?;
                }
            }
        }

        Ok(Response::new(PushResponse {
            accepted: true,
            message: "ok".into(),
        }))
    }
}

async fn issue_token(db: &Db, user: &User, name: &str) -> Result<String, Status> {
    let token = auth::generate_token();
    let hash = auth::hash_token(&token);
    // CLI tokens do not expire by default.
    db.create_token(user.id, name, &hash, None)
        .await
        .map_err(internal)?;
    Ok(token)
}

/// The ids of every object reachable from `want` but not from `have`. The bytes
/// are streamed separately so they never all sit in memory at once.
fn compute_send_ids(
    store: &ObjectStore,
    want: &[String],
    have: &[String],
) -> anyhow::Result<Vec<ObjectId>> {
    let want_ids = parse_ids(want)?;
    let have_ids = parse_ids(have)?;

    let have_objects = chip_core::dag::reachable_objects(store, &have_ids)?;
    let send = chip_core::dag::reachable_objects(store, &want_ids)?;

    Ok(send.difference(&have_objects).copied().collect())
}

fn parse_ids(ids: &[String]) -> anyhow::Result<Vec<ObjectId>> {
    ids.iter()
        .map(|s| ObjectId::from_str(s).map_err(|e| anyhow::anyhow!(e.to_string())))
        .collect()
}

fn internal<E: std::fmt::Display>(e: E) -> Status {
    Status::internal(e.to_string())
}
