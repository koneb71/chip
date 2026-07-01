//! Server-rendered web UI: account pages, repo browsing, change/diff views.
//!
//! HTML is rendered inline (small enough not to warrant a template engine) via
//! the [`page`] wrapper.

use std::str::FromStr;
use std::sync::Arc;

use axum::extract::{Form, Path, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use chip_core::dag;
use chip_core::diff;
use chip_core::hash::ObjectId;
use chip_core::object::EntryKind;
use chip_core::store::ObjectStore;
use serde::Deserialize;

use crate::auth;
use crate::config::Config;
use crate::db::{Db, Role, User};
use crate::store::StoreFactory;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub stores: StoreFactory,
    pub config: Arc<Config>,
    pub limiter: Arc<crate::ratelimit::RateLimiter>,
    pub tokens: Arc<crate::cache::TokenCache>,
    pub renders: Arc<crate::render_cache::RenderCache>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/docs", get(docs))
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/login", get(login_form).post(login_submit))
        .route("/register", get(register_form).post(register_submit))
        .route("/logout", post(logout))
        .route("/new", get(new_repo_form).post(new_repo_submit))
        .route("/settings/tokens", get(tokens_page).post(create_token))
        .route("/settings/tokens/revoke", post(revoke_token))
        .route("/settings/keys", get(keys_page).post(add_key))
        .route("/settings/keys/revoke", post(revoke_key))
        .route("/:owner/:repo", get(repo_overview))
        .route("/:owner/:repo/collaborators", post(add_collaborator))
        .route("/:owner/:repo/change/:id", get(change_view))
        .route("/:owner/:repo/tree/:rev", get(tree_root))
        .route("/:owner/:repo/tree/:rev/*path", get(tree_sub))
        .route("/:owner/:repo/blob/:rev/*path", get(blob_view))
        .route("/:owner/:repo/history/:rev/*path", get(file_history))
        .route("/:owner/:repo/requests", get(requests_list))
        .route(
            "/:owner/:repo/requests/new",
            get(new_request_form).post(new_request_submit),
        )
        .route(
            "/:owner/:repo/requests/:num",
            get(request_detail).post(comment_submit),
        )
        .route("/:owner/:repo/requests/:num/merge", post(merge_submit))
        .route("/:owner/:repo/requests/:num/review", post(review_submit))
        .layer(middleware::from_fn(security_headers))
        .with_state(state)
}

/// Add hardening headers to every web response. The UI is fully self-contained
/// (inline styles + one inline copy handler, no external resources), so a tight
/// CSP applies.
async fn security_headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    h.insert("x-frame-options", HeaderValue::from_static("DENY"));
    h.insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    h.insert(
        "content-security-policy",
        HeaderValue::from_static(
            "default-src 'none'; style-src 'unsafe-inline'; img-src data:; \
             form-action 'self'; base-uri 'none'; frame-ancestors 'none'; \
             script-src 'unsafe-inline'",
        ),
    );
    res
}

// --- helpers ---------------------------------------------------------------

/// The raw session token from the request cookies, if present.
fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
    cookie
        .split(';')
        .filter_map(|c| c.trim().strip_prefix("chip_session="))
        .next()
        .map(|s| s.to_string())
}

async fn current_user(state: &AppState, headers: &HeaderMap) -> Option<User> {
    let token = session_token(headers)?;
    let hash = auth::hash_token(&token);
    state.tokens.user_for_token(&hash).await.ok().flatten()
}

/// A CSRF token bound to both the session and the server secret. An attacker
/// cannot forge it without the secret, nor derive it without the (HttpOnly)
/// session cookie. Reuses chip-core's BLAKE3 hashing.
fn csrf_for(secret: &str, session: &str) -> String {
    chip_core::hash::ObjectId::hash(format!("{secret}::csrf::{session}").as_bytes()).to_hex()
}

/// The CSRF token for the current request's session, if logged in.
fn csrf_of(state: &AppState, headers: &HeaderMap) -> Option<String> {
    session_token(headers).map(|t| csrf_for(&state.config.secret, &t))
}

/// Verify a submitted CSRF token against the request's session. Returns false
/// when no session, or the token is missing/incorrect.
fn csrf_ok(state: &AppState, headers: &HeaderMap, submitted: &str) -> bool {
    match csrf_of(state, headers) {
        Some(expected) => {
            // constant-time-ish comparison
            expected.len() == submitted.len()
                && expected
                    .bytes()
                    .zip(submitted.bytes())
                    .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                    == 0
        }
        None => false,
    }
}

fn csrf_input(csrf: Option<&str>) -> String {
    match csrf {
        Some(c) => format!("<input type=\"hidden\" name=\"_csrf\" value=\"{c}\">"),
        None => String::new(),
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn page(title: &str, user: Option<&User>, body: &str) -> Html<String> {
    let nav_user = match user {
        Some(u) => {
            let initial = esc(&u.username.chars().next().unwrap_or('?').to_string());
            format!(
                "<a href=\"/settings/tokens\">Tokens</a>\
                 <a href=\"/settings/keys\">SSH keys</a>\
                 <span class=\"who\"><span class=\"ava\">{initial}</span>\
                 <span class=\"uname\">{name}</span></span>\
                 <form method=\"post\" action=\"/logout\" style=\"display:inline\">\
                 <button type=\"submit\" class=\"btn btn-ghost btn-sm\">Log out</button></form>",
                name = esc(&u.username)
            )
        }
        None => "<a href=\"/login\">Log in</a>\
             <a class=\"btn btn-primary btn-sm\" href=\"/register\">Sign up</a>"
            .to_string(),
    };
    Html(format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
         <title>{title} · chip</title>\
         <style>{CSS}</style></head><body>\
         <header class=\"nav\"><div class=\"nav-inner\">\
         <a class=\"brand\" href=\"/\"><span class=\"logo\">◆</span> chip</a>\
         <nav class=\"nav-links\"><a href=\"/docs\">Docs</a>{nav_user}</nav>\
         </div></header>\
         <main class=\"container fade-up\">{body}</main>\
         <footer class=\"foot\"><div class=\"foot-inner\">\
         <span>chip · a changeset-oriented version control system</span>\
         <span><a href=\"/docs\">Docs</a> · \
         <a href=\"https://github.com/koneb71/chip\">GitHub</a></span>\
         </div></footer></body></html>"
    ))
}

/// A page header: title (left, with optional subtitle) + right-aligned actions.
/// `title`/`subtitle`/`actions` are caller-escaped HTML fragments.
fn page_head(title: &str, subtitle: &str, actions: &str) -> String {
    let sub = if subtitle.is_empty() {
        String::new()
    } else {
        format!("<p class=\"sub\">{subtitle}</p>")
    };
    format!(
        "<div class=\"page-head\"><div><h1>{title}</h1>{sub}</div>\
         <div class=\"actions\">{actions}</div></div>"
    )
}

/// The Settings tab bar; `active` is "tokens" or "keys".
fn settings_subnav(active: &str) -> String {
    let cls = |n: &str| if n == active { " class=\"active\"" } else { "" };
    format!(
        "<nav class=\"subnav\"><a href=\"/settings/tokens\"{t}>API tokens</a>\
         <a href=\"/settings/keys\"{k}>SSH keys</a></nav>",
        t = cls("tokens"),
        k = cls("keys")
    )
}

/// Minimal black & white stylesheet for the whole web UI.
const CSS: &str = r#"
:root{
  --ink:#111111;--ink-soft:#3a3a3a;--muted:#6b6b6b;
  --line:#e6e6e6;--bg:#ffffff;--surface:#ffffff;--soft:#f6f6f6;
  --accent:#4f46e5;--accent-strong:#4338ca;--accent-soft:#eef2ff;--on-accent:#ffffff;
  --radius:14px;--shadow:0 4px 16px rgba(0,0,0,.06);--shadow-lg:0 10px 28px rgba(0,0,0,.12);
}
*{box-sizing:border-box}
html{scroll-behavior:smooth}
body{margin:0;color:var(--ink);background:var(--bg);
  font-family:'Inter',-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;
  line-height:1.55;-webkit-font-smoothing:antialiased}
h1{font-size:1.9rem;font-weight:700;letter-spacing:-.02em;margin:.2rem 0 1rem}
h2{font-size:1.35rem;font-weight:700;letter-spacing:-.01em;margin:1.6rem 0 .6rem}
h3{font-size:1.05rem;font-weight:600;margin:1.4rem 0 .5rem;color:var(--ink-soft)}
p{margin:.5rem 0}
a{color:var(--ink);text-decoration:none;transition:opacity .15s ease,color .15s ease}
a:hover{opacity:.6}
code{background:var(--soft);padding:.1rem .35rem;border-radius:6px;font-size:.85em}
.muted{color:var(--muted)}
.link,.prose a{color:var(--accent);font-weight:500}
.link:hover,.prose a:hover{color:var(--accent-strong);opacity:1}

/* nav */
.nav{position:sticky;top:0;z-index:20;background:rgba(255,255,255,.85);
  backdrop-filter:saturate(180%) blur(12px);border-bottom:1px solid var(--line)}
.nav-inner{max-width:960px;margin:0 auto;padding:.85rem 1.25rem;display:flex;
  align-items:center;justify-content:space-between}
.brand{display:flex;align-items:center;gap:.4rem;font-weight:800;font-size:1.3rem;
  color:var(--ink);letter-spacing:-.02em}
.brand .logo{transition:transform .4s cubic-bezier(.2,.7,.2,1)}
.brand:hover .logo{transform:rotate(90deg) scale(1.1)}
.nav-links{display:flex;align-items:center;gap:1.1rem}
.nav-links a{color:var(--ink);font-weight:600;font-size:.95rem}
.nav-links a:hover{opacity:.6}
.nav-links a.active{color:var(--accent)}
.nav-links a.btn-primary{color:var(--on-accent)}
.nav-links a.btn-primary:hover{opacity:1}
.who{display:inline-flex;align-items:center;gap:.45rem;font-weight:600;color:var(--muted)}
.who .ava{display:inline-flex;align-items:center;justify-content:center;width:1.7rem;height:1.7rem;
  flex:0 0 auto;border-radius:50%;background:var(--ink);color:#fff;font-size:.8rem;font-weight:700;text-transform:uppercase}

/* layout */
.container{max-width:960px;margin:0 auto;padding:2rem 1.25rem 4rem}

/* cards */
.card{background:var(--surface);border:1px solid var(--line);border-radius:var(--radius);
  padding:1.25rem 1.4rem;box-shadow:var(--shadow);
  transition:transform .22s cubic-bezier(.2,.7,.2,1),box-shadow .22s ease,border-color .2s ease}
.card:hover{transform:translateY(-4px);box-shadow:var(--shadow-lg);border-color:#d0d0d0}
.cards{display:grid;grid-template-columns:repeat(auto-fill,minmax(270px,1fr));gap:1.1rem;margin-top:1rem}
.repo-card{display:block;color:inherit}
.repo-card:hover{opacity:1}
.repo-card .name{font-weight:700;font-size:1.1rem;color:var(--ink)}
.repo-card .repo-ico{color:var(--accent)}
.repo-card .desc{color:var(--ink-soft);font-size:.9rem;margin-top:.35rem;
  display:-webkit-box;-webkit-line-clamp:2;-webkit-box-orient:vertical;overflow:hidden}
.repo-card .meta{color:var(--muted);font-size:.85rem;margin-top:.55rem}
/* new-repo form */
.hint{font-size:.85rem;color:var(--muted);margin:.4rem 0 0}
.hint code{font-size:.82rem}
.vis-cards{display:flex;flex-direction:column;gap:.7rem;margin:.5rem 0 .2rem}
.vis-card{display:flex;align-items:center;gap:.8rem;border:1px solid var(--line);
  border-radius:var(--radius);padding:.85rem 1rem;cursor:pointer;
  transition:border-color .15s ease,background .15s ease}
.vis-card:hover{border-color:var(--ink-soft)}
.vis-card:has(input:checked){border-color:var(--accent);background:var(--accent-soft);
  box-shadow:0 0 0 1px var(--accent) inset}
.vis-card input[type=radio]{width:1.1rem;height:1.1rem;flex:0 0 auto;margin:0;accent-color:var(--accent)}
.vis-card .vc-text{display:flex;flex-direction:column;gap:.15rem}
.vis-card .vc-text strong{font-size:.95rem}
.vis-card .vc-text small{font-size:.83rem;color:var(--muted)}

/* auth / narrow forms */
.narrow{max-width:430px;margin:1.5rem auto}
form p{margin:.6rem 0}
label{font-weight:600;font-size:.9rem;display:block;margin-bottom:.25rem}
input:not([type=radio]):not([type=checkbox]),select,textarea{width:100%;font-size:1rem;
  font-family:inherit;padding:.7rem .85rem;border:1px solid #d4d4d4;border-radius:12px;
  background:#fff;color:var(--ink);transition:border-color .15s,box-shadow .15s}
input:focus,select:focus,textarea:focus{outline:none;border-color:var(--accent);
  box-shadow:0 0 0 3px var(--accent-soft)}
textarea{min-height:6.5rem;resize:vertical;line-height:1.5}
textarea.mono,input.mono{font-family:ui-monospace,SFMono-Regular,Menlo,monospace;font-size:.9rem}
::placeholder{color:#9a9a9a}

/* buttons */
button,.btn{display:inline-flex;align-items:center;justify-content:center;gap:.4rem;
  font-size:.95rem;font-weight:600;padding:.6rem 1.1rem;border-radius:999px;border:1px solid var(--ink);
  cursor:pointer;background:var(--ink);color:#fff;
  transition:transform .12s ease,box-shadow .2s ease,background .2s ease,color .2s ease}
button:hover,.btn:hover{background:#000;box-shadow:0 6px 16px rgba(0,0,0,.18)}
button:active,.btn:active{transform:scale(.96)}
button:focus-visible,.btn:focus-visible{outline:none;box-shadow:0 0 0 3px var(--accent-soft)}
.btn-primary{background:var(--accent);color:var(--on-accent);border-color:var(--accent)}
.btn-primary:hover{background:var(--accent-strong);border-color:var(--accent-strong)}
.btn-ghost{background:transparent;color:var(--ink);border-color:var(--line);box-shadow:none}
.btn-ghost:hover{background:var(--soft);color:var(--ink);box-shadow:none}
.btn-sm{padding:.35rem .8rem;font-size:.82rem}

/* chips / badges */
.chip{display:inline-flex;align-items:center;gap:.4rem;background:var(--soft);
  border:1px solid var(--line);border-radius:999px;padding:.3rem .75rem;font-size:.85rem;
  font-weight:600;margin:.2rem .35rem .2rem 0;color:var(--ink);
  transition:transform .15s,border-color .15s}
.chip:hover{transform:translateY(-1px);border-color:var(--ink)}
.chip-open{background:var(--accent-soft);border-color:var(--accent);color:var(--accent-strong)}
.chip-merged{background:#ecfdf3;border-color:#63b47f;color:#116932}
.tag{color:var(--ink)}
.cr-actions{display:flex;gap:.6rem;flex-wrap:wrap;align-items:center}
.cr-actions form{margin:0}

/* tables / lists */
table{border-collapse:collapse;width:100%}
td{padding:.55rem .6rem;border-bottom:1px solid var(--line)}
tr{transition:background .15s ease}
tbody tr:hover,table:not(.diff) tr:hover{background:var(--soft)}

/* diff (monochrome) */
.diffstat{margin:.4rem 0 1rem;font-size:.95rem}
.add{color:var(--ink);font-weight:700}.del{color:var(--muted);font-weight:700}
.filediff{border:1px solid var(--line);border-radius:14px;margin:1.1rem 0;overflow:hidden;
  box-shadow:var(--shadow);transition:box-shadow .2s ease}
.filediff:hover{box-shadow:var(--shadow-lg)}
.fhead{background:var(--soft);padding:.65rem .9rem;font-family:ui-monospace,monospace;
  font-size:.9rem;display:flex;gap:.6rem;align-items:center;border-bottom:1px solid var(--line)}
.badge{display:inline-block;min-width:1.25rem;padding:0 .3rem;line-height:1.4rem;text-align:center;
  border-radius:6px;color:#fff;font-weight:800;font-size:.72rem}
.b-a{background:#111}.b-m{background:#777}.b-d{background:#cfcfcf;color:#333}
table.diff{width:100%;border-collapse:collapse;font-family:ui-monospace,SFMono-Regular,Menlo,monospace;
  font-size:.85rem;border:0}
table.diff td{border:0;padding:.05rem .6rem;white-space:pre;vertical-align:top}
table.diff tr:hover{background:transparent}
td.ln{width:1%;text-align:right;color:#b0b0b0;background:#fbfbfb;user-select:none;border-right:1px solid var(--line)}
tr.l-add{background:#efefef}tr.l-add td.code{color:#111}
tr.l-del{background:#fafafa}tr.l-del td.code{color:#9a9a9a;text-decoration:line-through}
tr.hunk td{background:#f3f3f3;color:#555;padding:.25rem .6rem;font-weight:600}

pre{background:var(--soft);padding:1rem;overflow:auto;border-radius:12px;border:1px solid var(--line)}

/* repo page */
.repo-head{display:flex;align-items:center;gap:1rem;flex-wrap:wrap;margin-bottom:.4rem}
.avatar{width:52px;height:52px;border-radius:14px;background:var(--ink);color:#fff;display:flex;
  align-items:center;justify-content:center;font-weight:700;font-size:1.4rem;flex:0 0 auto;text-transform:uppercase}
.metrics{display:flex;gap:2rem;margin:.5rem 0 1.5rem;flex-wrap:wrap}
.metric b{font-size:1.3rem;font-weight:700;line-height:1.1;display:block}
.metric span{font-size:.8rem;color:var(--muted)}
.section{margin-top:1.9rem}
.section-h{font-size:.78rem;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);
  font-weight:700;margin:0 0 .6rem}
.clone-box{display:flex;align-items:center;gap:.6rem;background:var(--soft);border:1px solid var(--line);
  border-radius:12px;padding:.55rem .55rem .55rem .85rem;font-family:ui-monospace,Menlo,monospace;
  font-size:.9rem;overflow:hidden}
.clone-box code{background:none;padding:0;white-space:nowrap;overflow:auto;flex:1 1 auto}
.copy-btn{margin-left:auto;flex:0 0 auto;padding:.35rem .8rem;font-size:.82rem}
.commit-list{border:1px solid var(--line);border-radius:14px;overflow:hidden;box-shadow:var(--shadow)}
.commit-row{display:flex;align-items:center;gap:1rem;padding:.8rem 1rem;text-decoration:none;
  color:inherit;border-bottom:1px solid var(--line);transition:background .15s ease}
.commit-row:last-child{border-bottom:0}
.commit-row:hover{background:var(--soft);opacity:1}
.hash{font-family:ui-monospace,Menlo,monospace;font-size:.78rem;background:#fff;border:1px solid var(--line);
  border-radius:6px;padding:.15rem .45rem;color:var(--muted);flex:0 0 auto}
.commit-main{flex:1 1 auto;min-width:0}
.commit-msg{font-weight:600;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.commit-sub{font-size:.76rem;color:var(--muted);font-family:ui-monospace,Menlo,monospace}
.commit-stat{flex:0 0 auto;font-size:.8rem;color:var(--muted);font-family:ui-monospace,Menlo,monospace}
.empty{border:1px dashed #d8d8d8;border-radius:14px;padding:1.6rem;text-align:center;color:var(--muted)}

/* file browser */
.crumbs{margin:.5rem 0 1rem;font-family:ui-monospace,Menlo,monospace;font-size:.9rem}
.crumbs a{color:var(--ink)}.crumbs span{color:var(--muted)}
.filelist{border:1px solid var(--line);border-radius:14px;overflow:hidden;box-shadow:var(--shadow)}
.filerow{display:block;padding:.6rem 1rem;text-decoration:none;color:inherit;border-bottom:1px solid var(--line);transition:background .15s ease}
.filerow:last-child{border-bottom:0}
.filerow:hover{background:var(--soft);opacity:1}
table.blob{width:100%;border-collapse:collapse;font-family:ui-monospace,Menlo,monospace;font-size:.85rem;border:1px solid var(--line);border-radius:14px;overflow:hidden}
table.blob td{border:0;padding:.05rem .6rem;white-space:pre;vertical-align:top}
table.blob td.ln{width:1%;text-align:right;color:#b0b0b0;background:#fbfbfb;border-right:1px solid var(--line);user-select:none}
table.blob tr:hover{background:var(--soft)}

/* page header (title + actions) */
.page-head{display:flex;align-items:flex-start;justify-content:space-between;gap:1rem;
  flex-wrap:wrap;margin-bottom:.4rem}
.page-head h1{margin:.1rem 0}
.page-head .sub{color:var(--muted);margin:.1rem 0 0}
.page-head .actions{display:flex;gap:.6rem;align-items:center;flex-shrink:0}

/* settings sub-nav */
.subnav{display:flex;gap:.4rem;border-bottom:1px solid var(--line);margin:1.1rem 0 1.6rem}
.subnav a{padding:.5rem .9rem;font-weight:600;font-size:.92rem;color:var(--muted);
  border-bottom:2px solid transparent;margin-bottom:-1px;border-radius:8px 8px 0 0}
.subnav a:hover{color:var(--ink);opacity:1;background:var(--soft)}
.subnav a.active{color:var(--accent);border-bottom-color:var(--accent)}

/* generic list (settings rows, etc.) */
.list{border:1px solid var(--line);border-radius:14px;overflow:hidden;box-shadow:var(--shadow)}
.list-row{display:flex;align-items:center;gap:1rem;padding:.85rem 1.1rem;
  border-bottom:1px solid var(--line)}
.list-row:last-child{border-bottom:0}
.list-row .lr-main{flex:1 1 auto;min-width:0}
.list-row .lr-name{font-weight:600}
.list-row .lr-sub{font-size:.82rem;color:var(--muted);margin-top:.1rem;
  font-family:ui-monospace,Menlo,monospace;overflow:hidden;text-overflow:ellipsis}
.list-row .lr-meta{flex:0 0 auto;font-size:.82rem;color:var(--muted);text-align:right}
.list-row form{margin:0}
.list-head{font-size:.78rem;text-transform:uppercase;letter-spacing:.06em;color:var(--muted);
  font-weight:700;margin:0 0 .6rem}

/* prose (docs) */
.prose{max-width:720px}
.prose h2{border-top:1px solid var(--line);padding-top:1.4rem}
.prose h2:first-of-type{border-top:0;padding-top:0}
.prose ul,.prose ol{padding-left:1.2rem}
.prose li{margin:.3rem 0}

/* rendered README */
.readme{padding:1.5rem 1.7rem}
.readme>:first-child{margin-top:0}
.readme h1{font-size:1.5rem}
.readme h2{font-size:1.25rem;border-top:1px solid var(--line);padding-top:1rem;margin-top:1.4rem}
.readme ul,.readme ol{padding-left:1.3rem}
.readme li{margin:.25rem 0}
.readme pre{overflow:auto}
.readme img{max-width:100%}
.readme a{color:var(--accent);font-weight:500}
.readme table{width:auto;border-collapse:collapse}
.readme th,.readme td{border:1px solid var(--line);padding:.4rem .65rem}
.readme blockquote{margin:.6rem 0;padding:.2rem 1rem;border-left:3px solid var(--line);color:var(--muted)}

/* token reveal */
.reveal{display:flex;align-items:center;gap:.6rem;background:var(--soft);
  border:1px solid var(--line);border-radius:12px;padding:.55rem .55rem .55rem .9rem;
  font-family:ui-monospace,Menlo,monospace;font-size:.9rem;word-break:break-all}
.reveal code{background:none;padding:0;flex:1 1 auto}

/* centered single-card pages (auth, 404) */
.center{min-height:60vh;display:flex;flex-direction:column;align-items:center;justify-content:center;text-align:center}
.center .card{width:100%;text-align:left}

/* footer */
.foot{border-top:1px solid var(--line);margin-top:2.5rem}
.foot-inner{max-width:960px;margin:0 auto;padding:1.4rem 1.25rem;display:flex;gap:1rem;
  flex-wrap:wrap;align-items:center;justify-content:space-between;color:var(--muted);font-size:.85rem}
.foot a{color:var(--muted);font-weight:600}.foot a:hover{color:var(--ink);opacity:1}

@media (max-width:560px){
  .container{padding:1.4rem 1rem 3rem}
  .nav-inner{flex-wrap:wrap;gap:.3rem .8rem;padding:.65rem 1rem}
  .nav-links{gap:.85rem;font-size:.9rem;flex-wrap:wrap}
  .nav-links a,.who{white-space:nowrap}
  .who .uname{display:none}
  .page-head{flex-direction:column}
  .narrow{margin:.5rem auto}
}

/* entrance animation */
@keyframes fadeUp{from{opacity:0;transform:translateY(14px)}to{opacity:1;transform:none}}
.fade-up{animation:fadeUp .5s cubic-bezier(.2,.7,.2,1) both}
@media (prefers-reduced-motion:reduce){*{animation:none!important;transition:none!important}}
"#;

fn session_cookie(token: &str, secure: bool) -> String {
    let secure = if secure { "; Secure" } else { "" };
    format!("chip_session={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age=2592000{secure}")
}

// --- handlers --------------------------------------------------------------

async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

/// Readiness probe: checks the database is reachable. Used by load balancers /
/// orchestrators to gate traffic.
async fn readyz(State(state): State<AppState>) -> impl IntoResponse {
    match state.db.ping().await {
        Ok(()) => (StatusCode::OK, "ready"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "db unavailable"),
    }
}

/// Public documentation page describing chip's model and CLI.
async fn docs(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = current_user(&state, &headers).await;
    let base = esc(state.config.base_url.trim_end_matches('/'));
    let owner = user
        .as_ref()
        .map(|u| esc(&u.username))
        .unwrap_or_else(|| "alice".to_string());

    let body = format!(
        r#"<h1>chip documentation</h1>
<p class="muted">chip is a changeset-oriented version control system — a Git
<em>alternative</em>, not a clone. This server hosts chip repositories and speaks
the chip sync protocol over gRPC.</p>

<h2>How chip differs from Git</h2>
<ul>
  <li><strong>No staging area.</strong> There is no <code>add</code>; the whole
      working tree is snapshotted on <code>commit</code>.</li>
  <li><strong>Stable change-ids.</strong> Every change has a change-id that
      <em>persists across rewrites</em> (amend/rebase), separate from its content
      (commit) hash.</li>
  <li><strong>First-class conflicts.</strong> A conflicting merge never aborts —
      it produces a conflicted change you resolve with a normal commit.</li>
  <li><strong>Universal undo.</strong> <code>chip undo</code> reverses the last
      operation via an operation log.</li>
</ul>

<h2>Install the CLI</h2>
<p>Prebuilt <code>chip</code> binaries are published for macOS (arm64/x64), Linux
(x64/arm64, static musl), and Windows (x64) on each release — no Rust or
<code>protoc</code> needed.</p>
<pre># macOS / Linux
curl --proto '=https' --tlsv1.2 -LsSf \
  https://github.com/koneb71/chip/releases/latest/download/chip-cli-installer.sh | sh</pre>
<pre># Windows (PowerShell)
powershell -ExecutionPolicy Bypass -c "irm https://github.com/koneb71/chip/releases/latest/download/chip-cli-installer.ps1 | iex"</pre>
<p class="muted">Or build from source (needs Rust and <code>protoc</code>):
<code>cargo install --path crates/chip-cli</code>.</p>

<h2>Getting started</h2>
<ol>
  <li><a href="/register">Create an account</a> (or <a href="/login">log in</a>).</li>
  <li>Pick how the CLI authenticates:
    <ul>
      <li><strong>HTTP</strong> — create an <a href="/settings/tokens">API token</a>
          and run <code>chip login</code> (the token is stored locally).</li>
      <li><strong>SSH</strong> — add your public key under
          <a href="/settings/keys">SSH keys</a>; no token needed.</li>
    </ul>
  </li>
  <li>Create a repository from <a href="/new">New repository</a> (or
      <code>chip repo create</code>), then push to it.</li>
</ol>
<pre>chip register {base} -u {owner} -e you@example.com   # or: chip login {base} -u {owner}
chip clone {base}/{owner}/&lt;repo&gt;                      # over HTTP (bearer token)
chip clone ssh://chip@&lt;host&gt;/{owner}/&lt;repo&gt;            # over SSH (your key)</pre>

<h2>Everyday workflow</h2>
<p>There is no staging step — edit files, then commit the whole tree.</p>
<pre>chip init                     # start a repository
chip commit -m "message"      # snapshot the working tree as a new change
chip status                   # what changed since the last commit
chip diff                     # unified diff of those changes
chip log                      # history, organized by change-id
chip show [rev]               # a change's metadata + diff (default: @)
chip undo                     # reverse the last operation</pre>

<h2>Branching &amp; history</h2>
<pre>chip bookmark &lt;name&gt;          # create/move a bookmark (named branch)
chip checkout &lt;name|commit&gt;   # switch and update the working tree
chip checkout -b &lt;name&gt;       # create a bookmark at HEAD and switch
chip merge &lt;name|commit&gt;      # three-way merge (conflicts stay first-class)
chip rebase &lt;name|commit&gt;     # replay the branch onto a new base (keeps change-ids)
chip cherry-pick &lt;rev&gt;        # copy one commit's change onto the current change
chip amend [-m msg]           # rewrite the current change, keeping its change-id
chip resolve                  # clear resolved conflict markers
chip stack                    # show the stack of changes above the trunk
chip evolution [rev]          # a change's versions over time (across amend/rebase)</pre>
<p class="muted">A <code>rev</code> may be a bookmark, tag, <code>@</code>, or a
commit id — abbreviated ids (the 12-char prefix in <code>chip log</code>) work too.</p>

<h2>Reading history (for AI agents)</h2>
<p>History and diffs have token-efficient, machine-readable modes (color is
auto-disabled when output isn't a terminal):</p>
<pre>chip log --oneline            # one dense line per change
chip log --format json        # compact JSON array of changes + stats
chip show &lt;rev&gt; --stat         # per-file +/- summary, no line content
chip show &lt;rev&gt; --format json  # structured diff summary (--patch adds line content)
chip diff --format json        # same, for the working tree
chip status --format json      # working-tree changes as [{{status, path}}]</pre>

<h2>Browsing on the web</h2>
<p>A repository's page shows its bookmarks, tags, recent changes, and a rendered
<code>README.md</code>. From there:</p>
<ul>
  <li><strong>Browse files</strong> — navigate the tree and open blobs with
      <strong>syntax highlighting</strong>.</li>
  <li>Open any change to see its <strong>diff</strong>; open a file's
      <strong>History</strong> to see the commits that changed it.</li>
  <li>URLs take a revision (bookmark, tag, or commit id), e.g.
      <code>/{owner}/&lt;repo&gt;/tree/main</code>.</li>
</ul>

<h2>Change requests</h2>
<p>Propose merging one bookmark into another on a repository's <strong>Change
requests</strong> page: review the combined diff, comment, approve or request
changes, and merge in the browser. Merges are three-way and keep conflicts
first-class (a conflicting merge is surfaced, not forced).</p>

<h2>Coming from Git</h2>
<pre>chip import git &lt;path&gt; [dir]  # import a local Git repo's full history into chip</pre>

<h2>Working with this server</h2>
<pre>chip remote add origin {base}/{owner}/&lt;repo&gt;
chip push origin [--force]    # --force allows a non-fast-forward update
chip pull origin              # fast-forward; warns (never clobbers) on divergence
chip pull origin --rebase     # on divergence, rebase local changes onto the remote
chip pull origin --merge      # on divergence, create a merge commit</pre>
<p class="muted">The first push to a repository under your own account creates it
automatically. HTTP remotes use a bearer <a href="/settings/tokens">token</a>; SSH
remotes use your <a href="/settings/keys">key</a>.</p>

<h2>Settings</h2>
<ul>
  <li><a href="/settings/tokens">API tokens</a> — create and revoke tokens that
      authenticate the CLI over HTTP (optionally set an expiry).</li>
  <li><a href="/settings/keys">SSH keys</a> — add your public key to clone, push,
      and pull over SSH without a token.</li>
</ul>

<h2>Access &amp; security</h2>
<p>Repositories are public or private, with read/write collaborators managed by
the owner on the repository page. Repository <strong>object data is encrypted at
rest</strong> (AES-256-GCM). Passwords are Argon2-hashed; CLI tokens are stored
only as hashes (revocable, optionally expiring); failed logins are rate-limited;
and pushes are fast-forward unless forced.</p>
"#
    );

    page(
        "docs",
        user.as_ref(),
        &format!("<div class=\"prose\">{body}</div>"),
    )
    .into_response()
}

async fn index(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = current_user(&state, &headers).await;
    let repos = state
        .db
        .list_visible_repos(user.as_ref().map(|u| u.id))
        .await
        .unwrap_or_default();
    let actions = if user.is_some() {
        "<a class=\"btn btn-primary\" href=\"/new\">+ New repository</a>"
    } else {
        ""
    };
    let mut body = page_head(
        "Repositories",
        "Browse and manage repositories hosted on this server.",
        actions,
    );

    if repos.is_empty() {
        let cta = if user.is_some() {
            "<p style=\"margin-top:1rem\"><a class=\"btn btn-primary\" href=\"/new\">Create a repository</a></p>"
        } else {
            "<p class=\"muted\">Nothing public to show yet. <a class=\"link\" href=\"/login\">Log in</a> to create one.</p>"
        };
        body.push_str(&format!(
            "<div class=\"empty\" style=\"padding:2.4rem\">\
             <p style=\"font-size:1.1rem;font-weight:600;color:var(--ink)\">No repositories yet</p>\
             <p class=\"muted\">Repositories you own, collaborate on, or that are public appear here.</p>{cta}</div>"
        ));
    } else {
        body.push_str("<div class=\"cards\">");
        for r in repos {
            let vis_chip = if r.visibility == "public" {
                "<span class=\"chip\">Public</span>"
            } else {
                "<span class=\"chip\">Private</span>"
            };
            let desc = if r.description.trim().is_empty() {
                String::new()
            } else {
                format!("<div class=\"desc\">{}</div>", esc(&r.description))
            };
            body.push_str(&format!(
                "<a class=\"card repo-card\" href=\"/{0}/{1}\">\
                 <div class=\"name\"><span class=\"repo-ico\">◆</span> {0}/{1}</div>{3}\
                 <div class=\"meta\">{2}</div></a>",
                esc(&r.owner),
                esc(&r.name),
                vis_chip,
                desc
            ));
        }
        body.push_str("</div>");
    }
    page("chip", user.as_ref(), &body).into_response()
}

async fn login_form(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = current_user(&state, &headers).await;
    let body = "<div class=\"center\"><div class=\"card narrow\"><h1>Welcome back</h1>\
        <p class=\"muted\">Log in to your chip account.</p><form method=\"post\">\
        <p><label>Username</label><input name=\"username\" placeholder=\"username\" autocomplete=\"username\" autofocus required></p>\
        <p><label>Password</label><input name=\"password\" type=\"password\" placeholder=\"••••••••\" autocomplete=\"current-password\" required></p>\
        <button type=\"submit\" class=\"btn-primary\" style=\"width:100%;margin-top:.6rem\">Log in</button></form>\
        <p class=\"muted\" style=\"margin-top:1.1rem;text-align:center\">New here? <a class=\"link\" href=\"/register\">Create an account</a></p></div></div>";
    page("login", user.as_ref(), body).into_response()
}

#[derive(Deserialize)]
struct Credentials {
    username: String,
    password: String,
}

async fn login_submit(State(state): State<AppState>, Form(form): Form<Credentials>) -> Response {
    if !state.limiter.allowed(&form.username).await {
        return error_page(&state, "too many failed login attempts; try again later");
    }
    let user = state
        .db
        .find_user_by_username(&form.username)
        .await
        .ok()
        .flatten()
        .filter(|u| auth::verify_password(&form.password, &u.password_hash));
    match user {
        Some(u) => {
            state.limiter.record_success(&form.username).await;
            match issue_web_token(&state.db, &u).await {
                Ok(token) => {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        header::SET_COOKIE,
                        session_cookie(&token, state.config.cookie_secure)
                            .parse()
                            .unwrap(),
                    );
                    (headers, Redirect::to("/")).into_response()
                }
                Err(_) => error_page(&state, "could not start session"),
            }
        }
        None => {
            state.limiter.record_failure(&form.username).await;
            error_page(&state, "invalid credentials")
        }
    }
}

#[derive(Deserialize)]
struct Registration {
    username: String,
    email: String,
    password: String,
}

async fn register_form(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let user = current_user(&state, &headers).await;
    let body = "<div class=\"center\"><div class=\"card narrow\"><h1>Create your account</h1>\
        <p class=\"muted\">Host and sync repositories with chip.</p><form method=\"post\">\
        <p><label>Username</label><input name=\"username\" placeholder=\"letters, digits, - or _\" autocomplete=\"username\" autofocus required></p>\
        <p><label>Email</label><input name=\"email\" type=\"email\" placeholder=\"you@example.com\" autocomplete=\"email\" required></p>\
        <p><label>Password</label><input name=\"password\" type=\"password\" placeholder=\"at least 8 characters\" autocomplete=\"new-password\" required></p>\
        <button type=\"submit\" class=\"btn-primary\" style=\"width:100%;margin-top:.6rem\">Create account</button></form>\
        <p class=\"muted\" style=\"margin-top:1.1rem;text-align:center\">Already have an account? <a class=\"link\" href=\"/login\">Log in</a></p></div></div>";
    page("register", user.as_ref(), body).into_response()
}

async fn register_submit(
    State(state): State<AppState>,
    Form(form): Form<Registration>,
) -> Response {
    if !crate::validate::valid_name(&form.username) {
        return error_page(
            &state,
            "username must be 1-64 chars of letters, digits, '-' or '_'",
        );
    }
    if form.password.len() < auth::MIN_PASSWORD_LEN {
        return error_page(&state, "password must be at least 8 characters");
    }
    if state
        .db
        .find_user_by_username(&form.username)
        .await
        .ok()
        .flatten()
        .is_some()
    {
        return error_page(&state, "username taken");
    }
    let hash = match auth::hash_password(&form.password) {
        Ok(h) => h,
        Err(_) => return error_page(&state, "internal error"),
    };
    let user = match state
        .db
        .create_user(&form.username, &form.email, &hash)
        .await
    {
        Ok(u) => u,
        Err(_) => return error_page(&state, "could not create account"),
    };
    match issue_web_token(&state.db, &user).await {
        Ok(token) => {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::SET_COOKIE,
                session_cookie(&token, state.config.cookie_secure)
                    .parse()
                    .unwrap(),
            );
            (headers, Redirect::to("/")).into_response()
        }
        Err(_) => error_page(&state, "could not start session"),
    }
}

async fn logout() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::SET_COOKIE,
        "chip_session=; Path=/; Max-Age=0".parse().unwrap(),
    );
    (headers, Redirect::to("/")).into_response()
}

#[derive(Deserialize)]
struct NewRepo {
    name: String,
    visibility: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    _csrf: String,
}

/// Cap on a repository description (characters).
const MAX_DESCRIPTION_LEN: usize = 200;

async fn new_repo_form(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    let base = esc(state.config.base_url.trim_end_matches('/'));
    let owner = esc(&user.username);
    let body = format!(
        "<div class=\"card narrow\"><h1>New repository</h1>\
        <p class=\"muted\">Name it, add an optional description, and choose who can see it.</p>\
        <form method=\"post\">{csrf}\
        <p><label>Repository name</label>\
        <input id=\"repo-name\" name=\"name\" placeholder=\"my-project\" \
         pattern=\"[A-Za-z0-9_-]{{1,64}}\" autocomplete=\"off\" autofocus required></p>\
        <p class=\"hint\">Letters, digits, hyphens, and underscores · 1–64 characters.</p>\
        <p class=\"hint\">Will be created at <code>{base}/{owner}/<span id=\"repo-url-name\">…</span></code></p>\
        <p><label>Description <span class=\"muted\">(optional)</span></label>\
        <input name=\"description\" maxlength=\"{maxlen}\" placeholder=\"A short summary of this repository\"></p>\
        <label>Visibility</label>\
        <div class=\"vis-cards\">\
          <label class=\"vis-card\"><input type=\"radio\" name=\"visibility\" value=\"private\" checked>\
            <span class=\"vc-text\"><strong>Private</strong><small>Only you and collaborators can see it.</small></span></label>\
          <label class=\"vis-card\"><input type=\"radio\" name=\"visibility\" value=\"public\">\
            <span class=\"vc-text\"><strong>Public</strong><small>Anyone can view it.</small></span></label>\
        </div>\
        <button type=\"submit\" class=\"btn-primary\" style=\"width:100%;margin-top:1rem\">Create repository</button>\
        </form></div>\
        <script>(function(){{var n=document.getElementById('repo-name'),\
        o=document.getElementById('repo-url-name');if(n&&o){{var f=function(){{\
        o.textContent=n.value||'…';}};n.addEventListener('input',f);f();}}}})();</script>",
        csrf = csrf_input(csrf_of(&state, &headers).as_deref()),
        base = base,
        owner = owner,
        maxlen = MAX_DESCRIPTION_LEN,
    );
    page("new repo", Some(&user), &body).into_response()
}

async fn new_repo_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<NewRepo>,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    if !crate::validate::valid_name(&form.name) {
        return error_page(
            &state,
            "repository name must be 1-64 chars of letters, digits, '-' or '_'",
        );
    }
    let description = form.description.trim();
    if description.chars().count() > MAX_DESCRIPTION_LEN {
        return error_page(&state, "description is too long (max 200 characters)");
    }
    let visibility = if form.visibility == "public" {
        "public"
    } else {
        "private"
    };
    match state
        .db
        .create_repo(user.id, &form.name, visibility, description)
        .await
    {
        Ok(_) => Redirect::to(&format!("/{}/{}", user.username, form.name)).into_response(),
        Err(_) => error_page(&state, "could not create repository (name taken?)"),
    }
}

async fn tokens_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    let tokens = state.db.list_tokens(user.id).await.unwrap_or_default();
    let csrf = csrf_of(&state, &headers);
    let csrf_field = csrf_input(csrf.as_deref());
    let mut body = format!(
        "{head}{subnav}\
         <div class=\"card\"><form method=\"post\" \
         style=\"display:flex;gap:.7rem;flex-wrap:wrap;align-items:flex-end;margin:0\">{csrf_field}\
         <div style=\"flex:1 1 14rem\"><label>Token name</label>\
         <input name=\"name\" placeholder=\"my-laptop\" autocomplete=\"off\" required></div>\
         <div style=\"flex:0 1 12rem\"><label>Expires (days)</label>\
         <input name=\"expires_days\" type=\"number\" min=\"1\" placeholder=\"optional\"></div>\
         <button type=\"submit\" class=\"btn-primary\">Create token</button></form></div>",
        head = page_head(
            "API tokens",
            "Tokens authenticate the <code>chip</code> CLI over HTTP.",
            ""
        ),
        subnav = settings_subnav("tokens"),
    );
    body.push_str("<p class=\"list-head\">Your tokens</p>");
    if tokens.is_empty() {
        body.push_str(
            "<div class=\"empty\">No tokens yet — create one above to authenticate the CLI.</div>",
        );
    } else {
        body.push_str("<div class=\"list\">");
        for t in tokens {
            body.push_str(&format!(
                "<div class=\"list-row\"><div class=\"lr-main\"><div class=\"lr-name\">{name}</div></div>\
                 <div class=\"lr-meta\">last used {used}<br>expires {exp}</div>\
                 <form method=\"post\" action=\"/settings/tokens/revoke\">{csrf}\
                 <input type=\"hidden\" name=\"name\" value=\"{namev}\">\
                 <button type=\"submit\" class=\"btn-ghost btn-sm\">Revoke</button></form></div>",
                name = esc(&t.name),
                used = t.last_used.map(fmt_ts).unwrap_or_else(|| "never".into()),
                exp = t.expires_at.map(fmt_ts).unwrap_or_else(|| "never".into()),
                namev = esc(&t.name),
                csrf = csrf_field,
            ));
        }
        body.push_str("</div>");
    }
    page("tokens", Some(&user), &body).into_response()
}

fn fmt_ts(t: time::OffsetDateTime) -> String {
    format!("{:04}-{:02}-{:02}", t.year(), t.month() as u8, t.day())
}

#[derive(Deserialize)]
struct TokenForm {
    name: String,
    #[serde(default)]
    expires_days: Option<i64>,
    #[serde(default)]
    _csrf: String,
}

async fn create_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    let expires_at = form
        .expires_days
        .filter(|d| *d > 0)
        .map(|d| time::OffsetDateTime::now_utc() + time::Duration::days(d));
    let token = auth::generate_token();
    let hash = auth::hash_token(&token);
    if state
        .db
        .create_token(user.id, &form.name, &hash, expires_at)
        .await
        .is_err()
    {
        return error_page(&state, "could not create token");
    }
    let body = format!(
        "{head}\
         <div class=\"card narrow\">\
         <p>Copy it now — for security, it <strong>won't be shown again</strong>.</p>\
         <div class=\"reveal\"><code>{tok}</code>\
         <button class=\"btn btn-ghost btn-sm\" \
         onclick=\"navigator.clipboard.writeText('{tok}');this.textContent='Copied'\">Copy</button></div>\
         <p style=\"margin-top:1rem\"><a class=\"link\" href=\"/settings/tokens\">← Back to tokens</a></p>\
         </div>",
        head = page_head("Token created", "", ""),
        tok = esc(&token),
    );
    page("token", Some(&user), &body).into_response()
}

async fn revoke_token(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<TokenForm>,
) -> Response {
    if let Some(user) = current_user(&state, &headers).await {
        if csrf_ok(&state, &headers, &form._csrf) {
            let _ = state.db.revoke_token(user.id, &form.name).await;
        }
    }
    Redirect::to("/settings/tokens").into_response()
}

// --- SSH keys ---------------------------------------------------------------

/// Compute the SHA256 fingerprint of an openssh public-key line.
fn ssh_fingerprint(line: &str) -> Option<String> {
    use russh::keys::ssh_key::{HashAlg, PublicKey};
    let key = PublicKey::from_openssh(line.trim()).ok()?;
    Some(key.fingerprint(HashAlg::Sha256).to_string())
}

async fn keys_page(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    let keys = state.db.list_ssh_keys(user.id).await.unwrap_or_default();
    let csrf = csrf_of(&state, &headers);
    let csrf_field = csrf_input(csrf.as_deref());
    let mut body = format!(
        "{head}{subnav}\
         <div class=\"card\"><form method=\"post\">{csrf_field}\
         <p><label>Name</label><input name=\"name\" placeholder=\"laptop\" autocomplete=\"off\" required></p>\
         <p><label>Public key</label>\
         <textarea name=\"public_key\" class=\"mono\" rows=\"4\" spellcheck=\"false\" \
         placeholder=\"ssh-ed25519 AAAA… you@host\" required></textarea></p>\
         <p class=\"hint\">Paste the contents of your public key file — e.g. \
         <code>cat ~/.ssh/id_ed25519.pub</code>.</p>\
         <button type=\"submit\" class=\"btn-primary\">Add key</button></form></div>",
        head = page_head(
            "SSH keys",
            "Add your public key to clone, push, and pull over SSH.",
            ""
        ),
        subnav = settings_subnav("keys"),
    );
    body.push_str("<p class=\"list-head\">Your keys</p>");
    if keys.is_empty() {
        body.push_str(
            "<div class=\"empty\">No SSH keys yet — add one above to use the SSH transport.</div>",
        );
    } else {
        body.push_str("<div class=\"list\">");
        for (name, fp) in keys {
            body.push_str(&format!(
                "<div class=\"list-row\"><div class=\"lr-main\">\
                 <div class=\"lr-name\">{name}</div><div class=\"lr-sub\">{fp}</div></div>\
                 <form method=\"post\" action=\"/settings/keys/revoke\">{csrf}\
                 <input type=\"hidden\" name=\"fingerprint\" value=\"{fpv}\">\
                 <button type=\"submit\" class=\"btn-ghost btn-sm\">Revoke</button></form></div>",
                name = esc(&name),
                fp = esc(&fp),
                fpv = esc(&fp),
                csrf = csrf_field,
            ));
        }
        body.push_str("</div>");
    }
    page("ssh keys", Some(&user), &body).into_response()
}

#[derive(Deserialize)]
struct KeyForm {
    name: String,
    public_key: String,
    #[serde(default)]
    _csrf: String,
}

async fn add_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<KeyForm>,
) -> Response {
    let Some(user) = current_user(&state, &headers).await else {
        return Redirect::to("/login").into_response();
    };
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    let Some(fingerprint) = ssh_fingerprint(&form.public_key) else {
        return error_page(&state, "could not parse that public key");
    };
    match state
        .db
        .add_ssh_key(user.id, &form.name, &fingerprint, form.public_key.trim())
        .await
    {
        Ok(()) => Redirect::to("/settings/keys").into_response(),
        Err(_) => error_page(&state, "could not add key (already registered?)"),
    }
}

#[derive(Deserialize)]
struct RevokeKeyForm {
    fingerprint: String,
    #[serde(default)]
    _csrf: String,
}

async fn revoke_key(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<RevokeKeyForm>,
) -> Response {
    if let Some(user) = current_user(&state, &headers).await {
        if csrf_ok(&state, &headers, &form._csrf) {
            let _ = state.db.delete_ssh_key(user.id, &form.fingerprint).await;
        }
    }
    Redirect::to("/settings/keys").into_response()
}

async fn repo_overview(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let user = current_user(&state, &headers).await;
    let Some(repo) = state.db.find_repo(&owner, &name).await.ok().flatten() else {
        return not_found(&state, user.as_ref());
    };
    if state
        .db
        .role_for(&repo, user.as_ref().map(|u| u.id))
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return not_found(&state, user.as_ref());
    }

    let bookmarks = state.db.list_refs(repo.id, false).await.unwrap_or_default();
    let tags = state.db.list_refs(repo.id, true).await.unwrap_or_default();

    let is_owner = user.as_ref().map(|u| u.id) == Some(repo.owner_id);

    // Load history up front so the metrics can show a change count. Cached by the
    // head commit (a content hash), so a page reload doesn't re-walk history.
    let store = state.stores.repo_store(&owner, &name).ok();
    let history = match (store.as_ref(), bookmarks.first()) {
        (Some(s), Some((_, head))) => match ObjectId::from_str(head) {
            Ok(h) => state
                .renders
                .history_or_else(h, || dag::history(s, h).unwrap_or_default()),
            Err(_) => std::sync::Arc::new(Vec::new()),
        },
        _ => std::sync::Arc::new(Vec::new()),
    };

    let vis_chip = if repo.visibility == "public" {
        "<span class=\"chip\">Public</span>"
    } else {
        "<span class=\"chip\">Private</span>"
    };
    let initial = owner.chars().next().unwrap_or('?').to_string();

    // Header: avatar + name + visibility, then metrics.
    let mut body = format!(
        "<div class=\"repo-head\"><div class=\"avatar\">{}</div>\
         <div><div style=\"display:flex;align-items:center;gap:.6rem;flex-wrap:wrap\">\
         <h1 style=\"margin:0\">{}/{}</h1>{}</div>\
         <div class=\"muted\" style=\"font-size:.9rem\">Owned by {}</div></div></div>\
         <div class=\"metrics\">\
         <div class=\"metric\"><b>{}</b><span>bookmarks</span></div>\
         <div class=\"metric\"><b>{}</b><span>tags</span></div>\
         <div class=\"metric\"><b>{}</b><span>changes</span></div></div>",
        esc(&initial),
        esc(&owner),
        esc(&name),
        vis_chip,
        esc(&owner),
        bookmarks.len(),
        tags.len(),
        history.len(),
    );

    if !repo.description.trim().is_empty() {
        body.push_str(&format!(
            "<p style=\"margin:-.2rem 0 1.1rem;font-size:1.05rem;color:var(--ink-soft)\">{}</p>",
            esc(&repo.description)
        ));
    }

    // Browse files at the default bookmark + change requests.
    body.push_str("<div style=\"display:flex;gap:.6rem;flex-wrap:wrap\">");
    if let Some((bn, _)) = bookmarks.first() {
        body.push_str(&format!(
            "<a class=\"btn btn-primary\" href=\"/{}/{}/tree/{}\">Browse files</a>",
            esc(&owner),
            esc(&name),
            esc(bn)
        ));
    }
    body.push_str(&format!(
        "<a class=\"btn btn-ghost\" href=\"/{}/{}/requests\">Change requests</a></div>",
        esc(&owner),
        esc(&name),
    ));

    // Clone box with a copy button.
    let clone_cmd = format!(
        "chip clone {}/{}/{}",
        state.config.base_url.trim_end_matches('/'),
        owner,
        name
    );
    body.push_str(&format!(
        "<div class=\"section\"><div class=\"section-h\">Clone</div>\
         <div class=\"clone-box\"><code>{}</code>\
         <button class=\"btn btn-ghost copy-btn\" \
         onclick=\"navigator.clipboard.writeText('{}');this.textContent='Copied'\">Copy</button></div></div>",
        esc(&clone_cmd),
        esc(&clone_cmd),
    ));

    // Rendered README from the tip commit, if present. Cached by the README blob
    // id (its content), so it isn't re-rendered on every page load.
    if let (Some(store), Some((_, tip))) = (store.as_ref(), history.first()) {
        if let Some((blob_id, md)) = read_readme(store, &tip.tree) {
            let key = format!("r:{}", blob_id.to_hex());
            let html = state.renders.get_html(&key).map_or_else(
                || {
                    let h = crate::highlight::render_markdown(&md);
                    state.renders.put_html(key, &h);
                    h
                },
                |h| h.to_string(),
            );
            body.push_str(&format!(
                "<div class=\"section\"><div class=\"section-h\">Readme</div>\
                 <div class=\"card readme\">{html}</div></div>"
            ));
        }
    }

    // Empty repository: show a push-to-here quickstart instead of bare empty lists.
    if bookmarks.is_empty() {
        let remote_url = format!(
            "{}/{}/{}",
            state.config.base_url.trim_end_matches('/'),
            owner,
            name
        );
        let push_cmd = format!("chip remote add origin {remote_url} && chip push origin");
        body.push_str(&format!(
            "<div class=\"section\"><div class=\"section-h\">Quick start</div>\
             <p class=\"muted\">This repository is empty. Push an existing project into it:</p>\
             <div class=\"clone-box\"><code>{cmd}</code>\
             <button class=\"btn btn-ghost copy-btn\" \
             onclick=\"navigator.clipboard.writeText('{cmd}');this.textContent='Copied'\">Copy</button></div>\
             <p class=\"muted\" style=\"margin-top:.6rem\">You can also create repositories from the CLI \
             with <code>chip repo create {url}</code>, or clone an existing one with \
             <code>chip clone {url}</code>.</p></div>",
            cmd = esc(&push_cmd),
            url = esc(&remote_url),
        ));
    }

    // Bookmarks & tags.
    body.push_str("<div class=\"section\"><div class=\"section-h\">Bookmarks</div>");
    if bookmarks.is_empty() {
        body.push_str("<p class=\"muted\">No bookmarks pushed yet.</p>");
    } else {
        for (bn, target) in &bookmarks {
            body.push_str(&format!(
                "<a class=\"chip\" href=\"/{}/{}/change/{}\">{} <span class=\"muted\">{}</span></a>",
                esc(&owner),
                esc(&name),
                esc(target),
                esc(bn),
                &target[..target.len().min(8)]
            ));
        }
    }
    body.push_str("</div>");
    if !tags.is_empty() {
        body.push_str("<div class=\"section\"><div class=\"section-h\">Tags</div>");
        for (tn, target) in &tags {
            body.push_str(&format!(
                "<a class=\"chip\" href=\"/{}/{}/change/{}\"><span class=\"tag\">{}</span> <span class=\"muted\">{}</span></a>",
                esc(&owner),
                esc(&name),
                esc(target),
                esc(tn),
                &target[..target.len().min(8)]
            ));
        }
        body.push_str("</div>");
    }

    // Recent changes as a styled list.
    body.push_str("<div class=\"section\"><div class=\"section-h\">Recent changes</div>");
    if history.is_empty() {
        body.push_str("<div class=\"empty\">No changes yet — push a commit to get started.</div>");
    } else if let Some(store) = store.as_ref() {
        body.push_str("<div class=\"commit-list\">");
        for (id, commit) in history.iter().take(20) {
            let commit_hex = id.to_hex();
            // Read-through cache: per-commit stats are immutable (commit id is a
            // content hash), so compute once and reuse forever (shared via DB).
            let (files, added, removed) = match state.db.get_commit_stat(repo.id, &commit_hex).await
            {
                Ok(Some(s)) => s,
                _ => {
                    let base_tree = commit
                        .parents
                        .first()
                        .and_then(|p| store.get_commit(p).ok())
                        .map(|c| c.tree);
                    let s = diff::diff_stat(store, base_tree.as_ref(), &commit.tree)
                        .unwrap_or_default();
                    let triple = (s.files as i32, s.added as i32, s.removed as i32);
                    let _ = state
                        .db
                        .put_commit_stat(repo.id, &commit_hex, triple.0, triple.1, triple.2)
                        .await;
                    triple
                }
            };
            body.push_str(&format!(
                "<a class=\"commit-row\" href=\"/{}/{}/change/{}\">\
                 <span class=\"hash\">{}</span>\
                 <div class=\"commit-main\"><div class=\"commit-msg\">{}</div>\
                 <div class=\"commit-sub\">change {}</div></div>\
                 <div class=\"commit-stat\">{} files <span class=\"add\">+{}</span> <span class=\"del\">-{}</span></div></a>",
                esc(&owner),
                esc(&name),
                commit_hex,
                id.short(),
                esc(commit.message.lines().next().unwrap_or("")),
                esc(&commit.change_id.to_string()),
                files,
                added,
                removed,
            ));
        }
        body.push_str("</div>");
    }
    body.push_str("</div>");

    // Owner-only collaborator management, in a card at the bottom.
    if is_owner {
        body.push_str(&format!(
            "<div class=\"section\"><div class=\"section-h\">Collaborators</div>\
             <div class=\"card\"><form method=\"post\" action=\"collaborators\" \
             style=\"display:flex;gap:.6rem;flex-wrap:wrap;align-items:center;margin:0\">{}\
             <input name=\"username\" placeholder=\"username\" required style=\"flex:1 1 12rem;width:auto\">\
             <select name=\"role\" style=\"width:auto\"><option value=\"read\">Read</option>\
             <option value=\"write\">Write</option></select>\
             <button type=\"submit\">Add</button></form></div></div>",
            csrf_input(csrf_of(&state, &headers).as_deref())
        ));
    }

    page(&format!("{owner}/{name}"), user.as_ref(), &body).into_response()
}

#[derive(Deserialize)]
struct CollaboratorForm {
    username: String,
    role: String,
    #[serde(default)]
    _csrf: String,
}

async fn add_collaborator(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<CollaboratorForm>,
) -> Response {
    let user = current_user(&state, &headers).await;
    let Some(repo) = state.db.find_repo(&owner, &name).await.ok().flatten() else {
        return not_found(&state, user.as_ref());
    };
    // Only the owner may manage collaborators.
    if user.as_ref().map(|u| u.id) != Some(repo.owner_id) {
        return error_page(&state, "only the owner can add collaborators");
    }
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    let Some(target) = state
        .db
        .find_user_by_username(&form.username)
        .await
        .ok()
        .flatten()
    else {
        return error_page(&state, "no such user");
    };
    let role = if form.role == "write" {
        Role::Write
    } else {
        Role::Read
    };
    if state
        .db
        .add_collaborator(repo.id, target.id, role)
        .await
        .is_err()
    {
        return error_page(&state, "could not add collaborator");
    }
    Redirect::to(&format!("/{owner}/{name}")).into_response()
}

/// Find and read a root-level README (`README.md`/`.markdown`/`README`) from a
/// tree, returning its blob id and UTF-8 text if present and decodable. The blob
/// id lets callers cache the rendered HTML.
fn read_readme(store: &ObjectStore, tree_id: &ObjectId) -> Option<(ObjectId, String)> {
    let tree = store.get_tree(tree_id).ok()?;
    let entry = tree.entries.iter().find(|e| {
        e.kind == EntryKind::Blob
            && matches!(
                e.name.to_ascii_lowercase().as_str(),
                "readme.md" | "readme.markdown" | "readme"
            )
    })?;
    let blob = store.get_blob(&entry.id).ok()?;
    Some((entry.id, String::from_utf8(blob.data).ok()?))
}

/// Render structured file diffs as an HTML diff view (summary + per-file tables).
fn render_diff_html(diffs: &[chip_core::diff::FileDiff]) -> String {
    use chip_core::diff::{FileStatus, LineKind};
    if diffs.is_empty() {
        return "<p class=\"muted\">No changes.</p>".to_string();
    }
    let total_add: usize = diffs.iter().map(|d| d.added).sum();
    let total_del: usize = diffs.iter().map(|d| d.removed).sum();
    let mut out = format!(
        "<p class=\"diffstat\"><strong>{} file(s) changed</strong> \
         <span class=\"add\">+{total_add}</span> <span class=\"del\">-{total_del}</span></p>",
        diffs.len()
    );
    for d in diffs {
        let (badge_class, letter) = match d.status {
            FileStatus::Added => ("b-a", 'A'),
            FileStatus::Modified => ("b-m", 'M'),
            FileStatus::Deleted => ("b-d", 'D'),
        };
        out.push_str(&format!(
            "<div class=\"filediff\"><div class=\"fhead\">\
             <span class=\"badge {badge_class}\">{letter}</span> <strong>{}</strong>",
            esc(&d.path)
        ));
        if !d.binary {
            out.push_str(&format!(
                " <span class=\"add\">+{}</span> <span class=\"del\">-{}</span>",
                d.added, d.removed
            ));
        }
        out.push_str("</div>");
        if d.binary {
            out.push_str(
                "<p class=\"muted\" style=\"padding:.5rem .75rem\">Binary file changed</p></div>",
            );
            continue;
        }
        out.push_str("<table class=\"diff\">");
        for hunk in &d.hunks {
            out.push_str(&format!(
                "<tr class=\"hunk\"><td colspan=\"3\">{}</td></tr>",
                esc(&hunk.header)
            ));
            for line in &hunk.lines {
                let (row_class, sign) = match line.kind {
                    LineKind::Insert => ("l-add", '+'),
                    LineKind::Delete => ("l-del", '-'),
                    LineKind::Context => ("l-ctx", ' '),
                };
                let old = line.old_no.map(|n| n.to_string()).unwrap_or_default();
                let new = line.new_no.map(|n| n.to_string()).unwrap_or_default();
                out.push_str(&format!(
                    "<tr class=\"{row_class}\"><td class=\"ln\">{old}</td><td class=\"ln\">{new}</td>\
                     <td class=\"code\">{sign}{}</td></tr>",
                    esc(&line.content)
                ));
            }
        }
        out.push_str("</table></div>");
    }
    out
}

async fn change_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, id)): Path<(String, String, String)>,
) -> Response {
    let user = current_user(&state, &headers).await;
    let Some(repo) = state.db.find_repo(&owner, &name).await.ok().flatten() else {
        return not_found(&state, user.as_ref());
    };
    if state
        .db
        .role_for(&repo, user.as_ref().map(|u| u.id))
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return not_found(&state, user.as_ref());
    }
    let store = match state.stores.repo_store(&owner, &name) {
        Ok(s) => s,
        Err(_) => return error_page(&state, "store error"),
    };
    let commit_id = match ObjectId::from_str(&id) {
        Ok(i) => i,
        Err(_) => return not_found(&state, user.as_ref()),
    };
    let commit = match store.get_commit(&commit_id) {
        Ok(c) => c,
        Err(_) => return not_found(&state, user.as_ref()),
    };

    // Diff against first parent (or empty tree for a root commit). The rendered
    // HTML is deterministic on (base tree, new tree), so it's cached by that key.
    let base_tree = commit
        .parents
        .first()
        .and_then(|p| store.get_commit(p).ok())
        .map(|c| c.tree);
    let key = format!(
        "d:{}:{}",
        base_tree.map(|t| t.to_hex()).unwrap_or_else(|| "-".into()),
        commit.tree.to_hex()
    );
    let diff_html = if let Some(h) = state.renders.get_html(&key) {
        h.to_string()
    } else {
        match diff::file_diffs(&store, base_tree.as_ref(), &commit.tree) {
            Ok(diffs) => {
                let html = render_diff_html(&diffs);
                state.renders.put_html(key, &html);
                html
            }
            Err(e) => {
                tracing::warn!(
                    "diff render failed for {owner}/{name}@{}: {e}",
                    commit_id.short()
                );
                "<p class=\"muted\">(diff unavailable)</p>".to_string()
            }
        }
    };

    let conflict_note = if commit.is_conflicted() {
        format!(
            "<p style=\"color:#b91c1c\">⚠ conflicted files: {}</p>",
            esc(&commit.conflicts.join(", "))
        )
    } else {
        String::new()
    };

    let body = format!(
        "<h1>change {}</h1><p>commit <code>{}</code> · \
         <a href=\"/{}/{}/tree/{}\">browse files at this change</a></p>\
         <p>{} · {}</p><p><strong>{}</strong></p>{}{}",
        esc(&commit.change_id.to_string()),
        commit_id.short(),
        esc(&owner),
        esc(&name),
        commit_id.to_hex(),
        esc(&commit.author),
        commit.timestamp,
        esc(commit.message.lines().next().unwrap_or("")),
        conflict_note,
        diff_html,
    );
    page("change", user.as_ref(), &body).into_response()
}

// --- File browser -----------------------------------------------------------

/// Resolve a revision (bookmark, tag, or commit id) to a commit id, then load
/// the object store + root tree, enforcing read access.
async fn browse_context(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    rev: &str,
) -> Result<BrowseContext, Response> {
    let user = current_user(state, headers).await;
    let Some(repo) = state.db.find_repo(owner, name).await.ok().flatten() else {
        return Err(not_found(state, user.as_ref()));
    };
    if state
        .db
        .role_for(&repo, user.as_ref().map(|u| u.id))
        .await
        .ok()
        .flatten()
        .is_none()
    {
        return Err(not_found(state, user.as_ref()));
    }
    // Resolve rev: bookmark, then tag, then a raw commit id.
    let commit_id = match state.db.get_ref(repo.id, false, rev).await.ok().flatten() {
        Some(t) => ObjectId::from_str(&t).ok(),
        None => match state.db.get_ref(repo.id, true, rev).await.ok().flatten() {
            Some(t) => ObjectId::from_str(&t).ok(),
            None => ObjectId::from_str(rev).ok(),
        },
    };
    let Some(commit_id) = commit_id else {
        return Err(not_found(state, user.as_ref()));
    };
    let Ok(store) = state.stores.repo_store(owner, name) else {
        return Err(error_page(state, "store error"));
    };
    let Ok(commit) = store.get_commit(&commit_id) else {
        return Err(not_found(state, user.as_ref()));
    };
    Ok(BrowseContext {
        store,
        commit_id,
        root: commit.tree,
        user,
    })
}

/// Resolved context for a browse request: the store, the resolved commit, its
/// root tree, and the (optional) viewer.
struct BrowseContext {
    store: ObjectStore,
    commit_id: ObjectId,
    root: ObjectId,
    user: Option<User>,
}

/// The blob id at `path` within a tree, or `None` if the path is absent or a dir.
fn blob_id_at(store: &ObjectStore, root: &ObjectId, path: &str) -> Option<ObjectId> {
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path),
    };
    let tree = walk_to_tree(store, root, dir)?;
    let entry = tree.get(file)?;
    (entry.kind == EntryKind::Blob).then_some(entry.id)
}

/// Navigate from the root tree down `path` (slash-separated directories).
fn walk_to_tree(
    store: &ObjectStore,
    root: &ObjectId,
    path: &str,
) -> Option<chip_core::object::Tree> {
    let mut tree = store.get_tree(root).ok()?;
    for comp in path.split('/').filter(|s| !s.is_empty()) {
        let entry = tree.get(comp)?;
        if entry.kind != EntryKind::Tree {
            return None;
        }
        tree = store.get_tree(&entry.id).ok()?;
    }
    Some(tree)
}

/// Breadcrumb of clickable path segments for a tree/blob view.
fn breadcrumb(owner: &str, name: &str, rev: &str, path: &str, is_blob: bool) -> String {
    let mut out = format!(
        "<div class=\"crumbs\"><a href=\"/{0}/{1}/tree/{2}\">{1}</a>",
        esc(owner),
        esc(name),
        esc(rev)
    );
    let comps: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut acc = String::new();
    for (i, comp) in comps.iter().enumerate() {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(comp);
        let last = i + 1 == comps.len();
        if last && is_blob {
            out.push_str(&format!(" / <span>{}</span>", esc(comp)));
        } else {
            out.push_str(&format!(
                " / <a href=\"/{}/{}/tree/{}/{}\">{}</a>",
                esc(owner),
                esc(name),
                esc(rev),
                esc(&acc),
                esc(comp)
            ));
        }
    }
    out.push_str("</div>");
    out
}

async fn tree_root(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev)): Path<(String, String, String)>,
) -> Response {
    render_tree(&state, &headers, &owner, &name, &rev, "").await
}

async fn tree_sub(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev, path)): Path<(String, String, String, String)>,
) -> Response {
    render_tree(&state, &headers, &owner, &name, &rev, &path).await
}

async fn render_tree(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    rev: &str,
    path: &str,
) -> Response {
    let BrowseContext {
        store, root, user, ..
    } = match browse_context(state, headers, owner, name, rev).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(tree) = walk_to_tree(&store, &root, path) else {
        return not_found(state, user.as_ref());
    };

    let mut body = format!(
        "<h1 style=\"font-size:1.4rem\">{}/{}</h1>\
         <p class=\"muted\">at <code>{}</code></p>{}",
        esc(owner),
        esc(name),
        esc(rev),
        breadcrumb(owner, name, rev, path, false)
    );

    body.push_str("<div class=\"filelist\">");
    // Parent link.
    if !path.is_empty() {
        let parent = path.rsplit_once('/').map(|(p, _)| p).unwrap_or("");
        let href = if parent.is_empty() {
            format!("/{owner}/{name}/tree/{rev}")
        } else {
            format!("/{owner}/{name}/tree/{rev}/{parent}")
        };
        body.push_str(&format!(
            "<a class=\"filerow\" href=\"{}\">📁 ..</a>",
            esc(&href)
        ));
    }
    // Directories first, then files (entries are already name-sorted).
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in &tree.entries {
        let child = if path.is_empty() {
            entry.name.clone()
        } else {
            format!("{path}/{}", entry.name)
        };
        match entry.kind {
            EntryKind::Tree => dirs.push(format!(
                "<a class=\"filerow\" href=\"/{}/{}/tree/{}/{}\">📁 {}</a>",
                esc(owner),
                esc(name),
                esc(rev),
                esc(&child),
                esc(&entry.name)
            )),
            EntryKind::Blob => files.push(format!(
                "<a class=\"filerow\" href=\"/{}/{}/blob/{}/{}\">📄 {}</a>",
                esc(owner),
                esc(name),
                esc(rev),
                esc(&child),
                esc(&entry.name)
            )),
        }
    }
    for row in dirs.into_iter().chain(files) {
        body.push_str(&row);
    }
    body.push_str("</div>");

    page(&format!("{owner}/{name}"), user.as_ref(), &body).into_response()
}

async fn blob_view(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev, path)): Path<(String, String, String, String)>,
) -> Response {
    let BrowseContext {
        store, root, user, ..
    } = match browse_context(&state, &headers, &owner, &name, &rev).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    // Split into parent dir + filename.
    let (dir, file) = match path.rsplit_once('/') {
        Some((d, f)) => (d, f),
        None => ("", path.as_str()),
    };
    let Some(tree) = walk_to_tree(&store, &root, dir) else {
        return not_found(&state, user.as_ref());
    };
    let Some(entry) = tree.get(file) else {
        return not_found(&state, user.as_ref());
    };
    if entry.kind != EntryKind::Blob {
        return not_found(&state, user.as_ref());
    }
    let Ok(blob) = store.get_blob(&entry.id) else {
        return not_found(&state, user.as_ref());
    };

    let mut body = format!(
        "<h1 style=\"font-size:1.4rem\">{}</h1>\
         <p class=\"muted\">at <code>{}</code> · \
         <a class=\"link\" href=\"/{}/{}/history/{}/{}\">History</a></p>{}",
        esc(file),
        esc(&rev),
        esc(&owner),
        esc(&name),
        esc(&rev),
        esc(&path),
        breadcrumb(&owner, &name, &rev, &path, true)
    );

    let is_binary = blob.data.iter().take(8192).any(|&b| b == 0);
    if is_binary {
        body.push_str(&format!(
            "<div class=\"empty\">Binary file ({} bytes)</div>",
            blob.data.len()
        ));
    } else {
        let text = String::from_utf8_lossy(&blob.data);
        // Syntax-highlight when we recognize the language; otherwise fall back to
        // plain, escaped line rendering. Highlighting is deterministic on the blob
        // content + filename, so it's cached by that key.
        let key = format!("b:{}:{}", entry.id.to_hex(), file);
        if let Some(html) = state.renders.get_html(&key) {
            body.push_str(&html);
        } else {
            match crate::highlight::blob_table(file, &text) {
                Some(html) => {
                    state.renders.put_html(key, &html);
                    body.push_str(&html);
                }
                None => {
                    body.push_str("<table class=\"blob\">");
                    for (i, line) in text.lines().enumerate() {
                        body.push_str(&format!(
                            "<tr><td class=\"ln\">{}</td><td class=\"code\">{}</td></tr>",
                            i + 1,
                            esc(line)
                        ));
                    }
                    body.push_str("</table>");
                }
            }
        }
    }

    page(file, user.as_ref(), &body).into_response()
}

/// History of a single file: the commits whose version of it differs from their
/// first parent's (i.e. that changed the file).
async fn file_history(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, rev, path)): Path<(String, String, String, String)>,
) -> Response {
    let BrowseContext {
        store,
        commit_id,
        user,
        ..
    } = match browse_context(&state, &headers, &owner, &name, &rev).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let history = state.renders.history_or_else(commit_id, || {
        dag::history(&store, commit_id).unwrap_or_default()
    });

    let mut rows = String::new();
    let mut count = 0;
    for (id, commit) in history.iter() {
        let Some(cur) = blob_id_at(&store, &commit.tree, &path) else {
            continue; // file not present here
        };
        let parent_blob = commit
            .parents
            .first()
            .and_then(|p| store.get_commit(p).ok())
            .and_then(|pc| blob_id_at(&store, &pc.tree, &path));
        // Include when the file was introduced (no parent version) or changed.
        if parent_blob == Some(cur) {
            continue;
        }
        count += 1;
        let date = time::OffsetDateTime::from_unix_timestamp(commit.timestamp)
            .map(fmt_ts)
            .unwrap_or_default();
        rows.push_str(&format!(
            "<a class=\"commit-row\" href=\"/{}/{}/change/{}\">\
             <span class=\"hash\">{}</span>\
             <div class=\"commit-main\"><div class=\"commit-msg\">{}</div>\
             <div class=\"commit-sub\">change {} · {}</div></div></a>",
            esc(&owner),
            esc(&name),
            id.to_hex(),
            id.short(),
            esc(commit.message.lines().next().unwrap_or("")),
            esc(&commit.change_id.to_string()),
            date,
        ));
    }

    let mut body = format!(
        "<h1 style=\"font-size:1.4rem\">History of {}</h1>\
         <p class=\"muted\">at <code>{}</code> · \
         <a class=\"link\" href=\"/{}/{}/blob/{}/{}\">View file</a></p>",
        esc(&path),
        esc(&rev),
        esc(&owner),
        esc(&name),
        esc(&rev),
        esc(&path),
    );
    if count == 0 {
        body.push_str("<div class=\"empty\">No history for this file.</div>");
    } else {
        body.push_str(&format!("<div class=\"commit-list\">{rows}</div>"));
    }
    page("file history", user.as_ref(), &body).into_response()
}

// --- Change requests --------------------------------------------------------

/// Load a repo and the viewer's role, or a not-found response. `None` role means
/// no access.
async fn repo_and_role(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
) -> Result<(crate::db::Repo, Option<Role>, Option<User>), Response> {
    let user = current_user(state, headers).await;
    let Some(repo) = state.db.find_repo(owner, name).await.ok().flatten() else {
        return Err(not_found(state, user.as_ref()));
    };
    let role = state
        .db
        .role_for(&repo, user.as_ref().map(|u| u.id))
        .await
        .ok()
        .flatten();
    if role.is_none() {
        return Err(not_found(state, user.as_ref()));
    }
    Ok((repo, role, user))
}

fn cr_state_chip(state: &str) -> &'static str {
    match state {
        "merged" => "<span class=\"chip chip-merged\">Merged</span>",
        "closed" => "<span class=\"chip\">Closed</span>",
        _ => "<span class=\"chip chip-open\">Open</span>",
    }
}

async fn requests_list(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let (repo, role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let crs = state
        .db
        .list_change_requests(repo.id)
        .await
        .unwrap_or_default();
    let actions = if role == Some(Role::Write) {
        format!("<a class=\"btn btn-primary\" href=\"/{owner}/{name}/requests/new\">+ New change request</a>")
    } else {
        String::new()
    };
    let mut body = page_head(
        &format!("Change requests · {}/{}", esc(&owner), esc(&name)),
        "Propose merging one bookmark into another, with review.",
        &actions,
    );
    if crs.is_empty() {
        body.push_str("<div class=\"empty\">No change requests yet.</div>");
    } else {
        body.push_str("<div class=\"list\">");
        for cr in crs {
            body.push_str(&format!(
                "<div class=\"list-row\"><div class=\"lr-main\">\
                 <div class=\"lr-name\"><a class=\"link\" href=\"/{o}/{n}/requests/{num}\">#{num} {title}</a></div>\
                 <div class=\"lr-sub\">{src} → {tgt} · by {author}</div></div>\
                 <div class=\"lr-meta\">{chip}</div></div>",
                o = esc(&owner),
                n = esc(&name),
                num = cr.number,
                title = esc(&cr.title),
                src = esc(&cr.source_ref),
                tgt = esc(&cr.target_ref),
                author = esc(&cr.author),
                chip = cr_state_chip(&cr.state),
            ));
        }
        body.push_str("</div>");
    }
    page("change requests", user.as_ref(), &body).into_response()
}

#[derive(Deserialize)]
struct NewCr {
    title: String,
    #[serde(default)]
    body: String,
    source: String,
    target: String,
    #[serde(default)]
    _csrf: String,
}

async fn new_request_form(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
) -> Response {
    let (repo, role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    if role != Some(Role::Write) {
        return error_page(&state, "you need write access to open a change request");
    }
    let bookmarks = state.db.list_refs(repo.id, false).await.unwrap_or_default();
    if bookmarks.len() < 2 {
        return error_page(
            &state,
            "a change request needs at least two bookmarks to compare",
        );
    }
    let opts = |sel: &str| -> String {
        bookmarks
            .iter()
            .map(|(bn, _)| {
                let s = if bn == sel { " selected" } else { "" };
                format!("<option value=\"{0}\"{1}>{0}</option>", esc(bn), s)
            })
            .collect()
    };
    let default_target = if bookmarks.iter().any(|(n, _)| n == "main") {
        "main"
    } else {
        bookmarks.first().map(|(n, _)| n.as_str()).unwrap_or("")
    };
    let body = format!(
        "{head}<div class=\"card narrow\"><form method=\"post\">{csrf}\
         <p><label>Title</label><input name=\"title\" placeholder=\"Short summary\" required></p>\
         <p><label>Description <span class=\"muted\">(optional)</span></label>\
         <textarea name=\"body\" placeholder=\"What does this change?\"></textarea></p>\
         <p><label>Merge from</label><select name=\"source\">{src_opts}</select></p>\
         <p><label>Into</label><select name=\"target\">{tgt_opts}</select></p>\
         <button type=\"submit\" class=\"btn-primary\">Create change request</button></form></div>",
        head = page_head("New change request", "", ""),
        csrf = csrf_input(csrf_of(&state, &headers).as_deref()),
        src_opts = opts(""),
        tgt_opts = opts(default_target),
    );
    page("new change request", user.as_ref(), &body).into_response()
}

async fn new_request_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name)): Path<(String, String)>,
    Form(form): Form<NewCr>,
) -> Response {
    let (repo, role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(user) = user else {
        return Redirect::to("/login").into_response();
    };
    if role != Some(Role::Write) {
        return error_page(&state, "you need write access to open a change request");
    }
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    let title = form.title.trim();
    if title.is_empty() {
        return error_page(&state, "title is required");
    }
    if form.source == form.target {
        return error_page(&state, "source and target must differ");
    }
    match state
        .db
        .create_change_request(
            repo.id,
            user.id,
            title,
            form.body.trim(),
            &form.source,
            &form.target,
        )
        .await
    {
        Ok(num) => Redirect::to(&format!("/{owner}/{name}/requests/{num}")).into_response(),
        Err(_) => error_page(&state, "could not create change request"),
    }
}

async fn request_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, num)): Path<(String, String, i32)>,
) -> Response {
    let (repo, role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(cr) = state
        .db
        .get_change_request(repo.id, num)
        .await
        .ok()
        .flatten()
    else {
        return not_found(&state, user.as_ref());
    };

    // Combined diff: what `source` would bring into `target`.
    let store = state.stores.repo_store(&owner, &name).ok();
    let src = state
        .db
        .get_ref(repo.id, false, &cr.source_ref)
        .await
        .ok()
        .flatten();
    let tgt = state
        .db
        .get_ref(repo.id, false, &cr.target_ref)
        .await
        .ok()
        .flatten();
    // Show what the CR *introduces*: a three-dot diff (merge-base → source), so
    // commits that landed on the target after branching don't show as deletions.
    let tree_of = |s: &ObjectStore, id: &ObjectId| -> Option<ObjectId> {
        s.get_commit(id).ok().map(|c| c.tree)
    };
    let cr_diff = |s: &ObjectStore, src_hex: &str, tgt_hex: &str| -> Option<String> {
        let src_id = ObjectId::from_str(src_hex).ok()?;
        let tgt_id = ObjectId::from_str(tgt_hex).ok()?;
        let src_tree = tree_of(s, &src_id)?;
        // Base = merge-base tree (falls back to the target tree, then empty).
        let base_tree = dag::merge_base(s, src_id, tgt_id)
            .ok()
            .flatten()
            .and_then(|b| tree_of(s, &b))
            .or_else(|| tree_of(s, &tgt_id));
        // Shares the change-view diff cache: same (base, new) trees → same HTML.
        let key = format!(
            "d:{}:{}",
            base_tree.map(|t| t.to_hex()).unwrap_or_else(|| "-".into()),
            src_tree.to_hex()
        );
        if let Some(h) = state.renders.get_html(&key) {
            return Some(h.to_string());
        }
        let diffs = diff::file_diffs(s, base_tree.as_ref(), &src_tree).ok()?;
        let html = render_diff_html(&diffs);
        state.renders.put_html(key, &html);
        Some(html)
    };
    let diff_html = match (store.as_ref(), &src, &tgt) {
        (Some(s), Some(src), Some(tgt)) => cr_diff(s, src, tgt)
            .unwrap_or_else(|| "<p class=\"muted\">Diff unavailable.</p>".into()),
        _ => "<p class=\"muted\">Diff unavailable (a bookmark may have been deleted).</p>".into(),
    };

    let reviews = state.db.list_cr_reviews(cr.id).await.unwrap_or_default();
    let comments = state.db.list_cr_comments(cr.id).await.unwrap_or_default();
    let is_writer = role == Some(Role::Write);
    let csrf = csrf_of(&state, &headers);

    let mut body = format!(
        "{head}<p class=\"muted\">{chip} · <code>{src}</code> → <code>{tgt}</code> · \
         by {author} · opened {date}</p>",
        head = page_head(&format!("#{} {}", cr.number, esc(&cr.title)), "", ""),
        chip = cr_state_chip(&cr.state),
        src = esc(&cr.source_ref),
        tgt = esc(&cr.target_ref),
        author = esc(&cr.author),
        date = fmt_ts(cr.created_at),
    );
    if !cr.body.trim().is_empty() {
        body.push_str(&format!("<p>{}</p>", esc(&cr.body)));
    }

    // Reviews summary.
    if !reviews.is_empty() {
        body.push_str("<div class=\"section\"><div class=\"section-h\">Reviews</div>");
        for (who, verdict) in &reviews {
            let label = if verdict == "approve" {
                "approved"
            } else {
                "requested changes"
            };
            body.push_str(&format!(
                "<span class=\"chip\">{} {}</span>",
                esc(who),
                label
            ));
        }
        body.push_str("</div>");
    }

    // Merge + review actions (writers only, open CRs).
    if is_writer && cr.state == "open" {
        let cf = csrf_input(csrf.as_deref());
        body.push_str(&format!(
            "<div class=\"section\"><div class=\"cr-actions\">\
             <form method=\"post\" action=\"/{o}/{n}/requests/{num}/merge\">{cf}\
             <button type=\"submit\" class=\"btn-primary\">Merge</button></form>\
             <form method=\"post\" action=\"/{o}/{n}/requests/{num}/review\">{cf}\
             <input type=\"hidden\" name=\"verdict\" value=\"approve\">\
             <button type=\"submit\" class=\"btn btn-ghost\">Approve</button></form>\
             <form method=\"post\" action=\"/{o}/{n}/requests/{num}/review\">{cf}\
             <input type=\"hidden\" name=\"verdict\" value=\"request_changes\">\
             <button type=\"submit\" class=\"btn btn-ghost\">Request changes</button></form>\
             </div></div>",
            o = esc(&owner),
            n = esc(&name),
        ));
    }

    // Comments.
    body.push_str("<div class=\"section\"><div class=\"section-h\">Discussion</div>");
    if comments.is_empty() {
        body.push_str("<p class=\"muted\">No comments yet.</p>");
    } else {
        for (who, text, when) in &comments {
            body.push_str(&format!(
                "<div class=\"card\" style=\"padding:.8rem 1rem;margin:.5rem 0\">\
                 <div class=\"lr-sub\" style=\"font-family:inherit\"><strong>{}</strong> · {}</div>\
                 <div style=\"margin-top:.3rem;white-space:pre-wrap\">{}</div></div>",
                esc(who),
                fmt_ts(*when),
                esc(text),
            ));
        }
    }
    if user.is_some() {
        body.push_str(&format!(
            "<form method=\"post\" style=\"margin-top:.6rem\">{}\
             <textarea name=\"body\" placeholder=\"Leave a comment\" required></textarea>\
             <button type=\"submit\" class=\"btn-primary\" style=\"margin-top:.4rem\">Comment</button></form>",
            csrf_input(csrf.as_deref())
        ));
    }
    body.push_str("</div>");

    // The diff.
    body.push_str("<div class=\"section\"><div class=\"section-h\">Changes</div>");
    body.push_str(&diff_html);
    body.push_str("</div>");

    page("change request", user.as_ref(), &body).into_response()
}

#[derive(Deserialize)]
struct CommentForm {
    body: String,
    #[serde(default)]
    _csrf: String,
}

async fn comment_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, num)): Path<(String, String, i32)>,
    Form(form): Form<CommentForm>,
) -> Response {
    let (repo, _role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(user) = user else {
        return Redirect::to("/login").into_response();
    };
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    if let Some(cr) = state
        .db
        .get_change_request(repo.id, num)
        .await
        .ok()
        .flatten()
    {
        let text = form.body.trim();
        if !text.is_empty() {
            let _ = state.db.add_cr_comment(cr.id, user.id, text).await;
        }
    }
    Redirect::to(&format!("/{owner}/{name}/requests/{num}")).into_response()
}

#[derive(Deserialize)]
struct ReviewForm {
    verdict: String,
    #[serde(default)]
    _csrf: String,
}

async fn review_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, num)): Path<(String, String, i32)>,
    Form(form): Form<ReviewForm>,
) -> Response {
    let (repo, role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(user) = user else {
        return Redirect::to("/login").into_response();
    };
    if role != Some(Role::Write) {
        return error_page(&state, "you need write access to review");
    }
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    let verdict = if form.verdict == "approve" {
        "approve"
    } else {
        "request_changes"
    };
    if let Some(cr) = state
        .db
        .get_change_request(repo.id, num)
        .await
        .ok()
        .flatten()
    {
        let _ = state.db.set_cr_review(cr.id, user.id, verdict).await;
    }
    Redirect::to(&format!("/{owner}/{name}/requests/{num}")).into_response()
}

#[derive(Deserialize)]
struct CsrfOnly {
    #[serde(default)]
    _csrf: String,
}

async fn merge_submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((owner, name, num)): Path<(String, String, i32)>,
    Form(form): Form<CsrfOnly>,
) -> Response {
    let (repo, role, user) = match repo_and_role(&state, &headers, &owner, &name).await {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let Some(user) = user else {
        return Redirect::to("/login").into_response();
    };
    if role != Some(Role::Write) {
        return error_page(&state, "you need write access to merge");
    }
    if !csrf_ok(&state, &headers, &form._csrf) {
        return error_page(&state, "invalid CSRF token");
    }
    let Some(cr) = state
        .db
        .get_change_request(repo.id, num)
        .await
        .ok()
        .flatten()
    else {
        return not_found(&state, Some(&user));
    };
    if cr.state != "open" {
        return Redirect::to(&format!("/{owner}/{name}/requests/{num}")).into_response();
    }
    let (Ok(store), Some(src), Some(tgt)) = (
        state.stores.repo_store(&owner, &name),
        state
            .db
            .get_ref(repo.id, false, &cr.source_ref)
            .await
            .ok()
            .flatten(),
        state
            .db
            .get_ref(repo.id, false, &cr.target_ref)
            .await
            .ok()
            .flatten(),
    ) else {
        return error_page(&state, "cannot merge: a bookmark is missing");
    };
    let msg = format!("merge change request #{num}: {}", cr.title);
    match crate::review::merge_refs(&store, &user.username, &msg, &src, &tgt) {
        Ok(summary) if summary.conflicts.is_empty() => {
            let hex = summary.commit.to_hex();
            let _ = state
                .db
                .set_ref(repo.id, false, &cr.target_ref, &hex)
                .await;
            let _ = state.db.set_cr_state(cr.id, "merged").await;
            Redirect::to(&format!("/{owner}/{name}/requests/{num}")).into_response()
        }
        Ok(_) => error_page(
            &state,
            "This merge has conflicts. Resolve them locally (chip merge, then push) — chip keeps conflicts first-class.",
        ),
        Err(_) => error_page(&state, "merge failed"),
    }
}

async fn issue_web_token(db: &Db, user: &User) -> anyhow::Result<String> {
    let token = auth::generate_token();
    let hash = auth::hash_token(&token);
    // Web sessions expire after 30 days (matching the cookie Max-Age).
    let expires_at = time::OffsetDateTime::now_utc() + time::Duration::days(30);
    db.create_token(user.id, "web-session", &hash, Some(expires_at))
        .await?;
    Ok(token)
}

fn error_page(_state: &AppState, msg: &str) -> Response {
    let body = format!(
        "<div class=\"center\"><div class=\"card narrow\" style=\"text-align:center\">\
         <h1>Something went wrong</h1><p class=\"muted\">{}</p>\
         <p style=\"margin-top:1rem\"><a class=\"btn btn-ghost\" href=\"/\">Back to repositories</a></p>\
         </div></div>",
        esc(msg)
    );
    (StatusCode::BAD_REQUEST, page("error", None, &body)).into_response()
}

fn not_found(_state: &AppState, user: Option<&User>) -> Response {
    let body = "<div class=\"center\"><div style=\"text-align:center\">\
        <h1 style=\"font-size:3rem;margin:0\">404</h1>\
        <p class=\"muted\">That page or repository doesn't exist, or you don't have access.</p>\
        <p style=\"margin-top:1rem\"><a class=\"btn btn-primary\" href=\"/\">Back to repositories</a></p>\
        </div></div>";
    (StatusCode::NOT_FOUND, page("not found", user, body)).into_response()
}
