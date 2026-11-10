//! Local web UI: an axum server bound to 127.0.0.1 that serves the embedded
//! page (`src/ui/index.html`) and a JSON API over [`crate::session`].
//!
//! Safety:
//! - The listener only ever binds the loopback address ([`bind`]).
//! - Requests whose `Host` is not `127.0.0.1` / `localhost` are refused, so
//!   a web page using DNS rebinding cannot drive the API.
//! - Every POST/PUT takes a JSON body (axum's `Json` extractor requires
//!   `Content-Type: application/json`), so a cross-site form or `no-cors`
//!   fetch cannot trigger a move without a CORS preflight, which this server
//!   never grants. `apply` additionally needs `{"confirm": true}`.
//! - Thumbnails and text previews are only served for files of the current
//!   scan (exact path match), so they cannot read anything else on disk.
//!
//! The server holds one session at a time and handles requests one by one
//! behind a mutex; this is a single-user local tool. The one exception is
//! `POST /api/ai/suggest`, which releases the mutex while the `claude` CLI
//! runs and discards the answer if the scan or settings changed meanwhile.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use axum::extract::{Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;

use crate::ai::{self, AiBackend, AiError, ClaudeCli};
use crate::apply::{ApplyReport, MoveError, MoveLog, UndoReport, apply, undo};
use crate::classify::FileInfo;
use crate::config::Config;
use crate::picker::{FolderPicker, Osascript};
use crate::plan::check_category_name;
use crate::preview::{self, MAX_THUMB_SIZE, PreviewError, QuickLook, TextPreview, ThumbnailBackend};
use crate::session::{self, PlanView, Session};

const INDEX_HTML: &str = include_str!("ui/index.html");

/// The only address the server ever listens on.
pub const BIND_IP: Ipv4Addr = Ipv4Addr::LOCALHOST;

/// Binds `127.0.0.1:<port>` (port 0 = any free port).
pub async fn bind(port: u16) -> std::io::Result<TcpListener> {
    TcpListener::bind(SocketAddr::from((BIND_IP, port))).await
}

pub struct AppState {
    config_path: PathBuf,
    log_path: PathBuf,
    ai: Arc<dyn AiBackend>,
    thumbs: Arc<dyn ThumbnailBackend>,
    picker: Arc<dyn FolderPicker>,
    inner: Mutex<Inner>,
}

struct Inner {
    config: Config,
    session: Option<Session>,
    /// Bumped whenever `session` is replaced by a new scan, so a slow AI
    /// call can tell its answer belongs to an older scan.
    generation: u64,
}

impl AppState {
    /// Loads the rules file (it must exist). The move log is created on the
    /// first apply. AI suggestions use the local `claude` CLI.
    pub fn new(config_path: &Path, log_path: &Path) -> Result<Arc<AppState>, String> {
        AppState::with_ai(config_path, log_path, Arc::new(ClaudeCli::default()))
    }

    /// Like [`AppState::new`] with another AI backend (tests use a mock).
    /// Thumbnails use Quick Look.
    pub fn with_ai(
        config_path: &Path,
        log_path: &Path,
        ai: Arc<dyn AiBackend>,
    ) -> Result<Arc<AppState>, String> {
        AppState::with_backends(config_path, log_path, ai, Arc::new(QuickLook::default()))
    }

    /// Like [`AppState::new`] with other AI and thumbnail backends.
    pub fn with_backends(
        config_path: &Path,
        log_path: &Path,
        ai: Arc<dyn AiBackend>,
        thumbs: Arc<dyn ThumbnailBackend>,
    ) -> Result<Arc<AppState>, String> {
        AppState::with_all(config_path, log_path, ai, thumbs, Arc::new(Osascript))
    }

    /// Like [`AppState::new`] with every backend replaced, including the
    /// folder picker (tests must never open the real dialog).
    pub fn with_all(
        config_path: &Path,
        log_path: &Path,
        ai: Arc<dyn AiBackend>,
        thumbs: Arc<dyn ThumbnailBackend>,
        picker: Arc<dyn FolderPicker>,
    ) -> Result<Arc<AppState>, String> {
        let config = Config::load(config_path).map_err(|e| e.to_string())?;
        Ok(Arc::new(AppState {
            config_path: config_path.to_path_buf(),
            log_path: log_path.to_path_buf(),
            ai,
            thumbs,
            picker,
            inner: Mutex::new(Inner {
                config,
                session: None,
                generation: 0,
            }),
        }))
    }

    fn lock(&self) -> Result<MutexGuard<'_, Inner>, ApiError> {
        self.inner
            .lock()
            .map_err(|_| ApiError::internal("server state is poisoned by an earlier panic; restart the server"))
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/state", get(get_state))
        .route("/api/config", get(get_config).put(put_config))
        .route("/api/scan", post(post_scan))
        .route("/api/pick-folder", post(post_pick_folder))
        .route("/api/plan", get(get_plan))
        .route("/api/clear", post(post_clear))
        .route("/api/assign", post(post_assign))
        .route("/api/assign-all", post(post_assign_all))
        .route("/api/check", post(post_check))
        .route("/api/rules/remember", post(post_remember))
        .route("/api/folders", post(post_add_folder))
        .route("/api/folders/rename", post(post_rename_folder))
        .route("/api/folders/delete", post(post_delete_folder))
        .route("/api/ai/suggest", post(post_ai_suggest))
        .route("/api/apply", post(post_apply))
        .route("/api/undo", post(post_undo))
        .route("/api/history", get(get_history))
        .route("/api/thumbnail", get(get_thumbnail))
        .route("/api/preview/text", get(get_text_preview))
        .layer(middleware::from_fn(require_local_host))
        .with_state(state)
}

/// Serves until the process is stopped.
pub async fn serve(listener: TcpListener, state: Arc<AppState>) -> std::io::Result<()> {
    axum::serve(listener, router(state)).await
}

async fn require_local_host(req: Request, next: Next) -> Response {
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or("");
    let name = match host.rsplit_once(':') {
        Some((name, port)) if port.chars().all(|c| c.is_ascii_digit()) => name,
        _ => host,
    };
    if name == "127.0.0.1" || name == "localhost" {
        next.run(req).await
    } else {
        ApiError::new(
            StatusCode::FORBIDDEN,
            format!("refusing request for host {host:?}; open http://127.0.0.1:<port>/"),
        )
        .into_response()
    }
}

// ---------- errors ----------

#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        ApiError {
            status,
            message: message.into(),
        }
    }
    fn bad(message: impl ToString) -> Self {
        ApiError::new(StatusCode::BAD_REQUEST, message.to_string())
    }
    fn internal(message: impl ToString) -> Self {
        ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(serde_json::json!({ "error": self.message })),
        )
            .into_response()
    }
}

impl From<session::SessionError> for ApiError {
    fn from(e: session::SessionError) -> Self {
        match e {
            session::SessionError::Metadata { .. } => ApiError::internal(e),
            _ => ApiError::bad(e),
        }
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

// ---------- views ----------

#[derive(Serialize)]
pub struct StateView {
    config: Config,
    /// `None` until a folder has been scanned.
    plan: Option<PlanView>,
}

fn state_view(inner: &Inner) -> Result<StateView, ApiError> {
    Ok(StateView {
        config: inner.config.clone(),
        plan: match &inner.session {
            Some(s) => Some(s.view(&inner.config)?),
            None => None,
        },
    })
}

fn session_ref(inner: &Inner) -> Result<&Session, ApiError> {
    inner
        .session
        .as_ref()
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "no folder scanned yet; scan one first"))
}

/// Runs a config edit on copies, saves the config, and only then commits
/// both, so a failed save leaves memory and disk in agreement.
fn edit_config(
    state: &AppState,
    edit: impl FnOnce(&mut Config, Option<&mut Session>) -> Result<(), ApiError>,
) -> ApiResult<StateView> {
    let mut inner = state.lock()?;
    let mut config = inner.config.clone();
    let mut session = inner.session.clone();
    edit(&mut config, session.as_mut())?;
    config.save(&state.config_path).map_err(ApiError::internal)?;
    inner.config = config;
    inner.session = session;
    Ok(Json(state_view(&inner)?))
}

// ---------- handlers ----------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn get_state(State(state): State<Arc<AppState>>) -> ApiResult<StateView> {
    let inner = state.lock()?;
    Ok(Json(state_view(&inner)?))
}

async fn get_config(State(state): State<Arc<AppState>>) -> ApiResult<Config> {
    Ok(Json(state.lock()?.config.clone()))
}

async fn put_config(
    State(state): State<Arc<AppState>>,
    Json(new): Json<Config>,
) -> ApiResult<StateView> {
    new.validate().map_err(ApiError::bad)?;
    for c in &new.categories {
        check_category_name(&c.name).map_err(ApiError::bad)?;
    }
    edit_config(&state, |config, session| {
        if let Some(s) = session {
            s.config_replaced(&new);
        }
        *config = new;
        Ok(())
    })
}

#[derive(Deserialize)]
struct ScanReq {
    dir: String,
    #[serde(default)]
    recursive: bool,
}

#[derive(Deserialize)]
struct PickFolderReq {
    /// The path in the box; the dialog starts there when it is a folder.
    #[serde(default)]
    start: String,
}

#[derive(Serialize)]
struct PickFolderView {
    /// `null` when the user pressed Cancel.
    path: Option<String>,
}

/// Opens the native folder dialog (see [`crate::picker`]). Holds no lock
/// while the dialog is open.
async fn post_pick_folder(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PickFolderReq>,
) -> ApiResult<PickFolderView> {
    let start = match req.start.trim() {
        "" => None,
        dir => Some(expand_home(dir)?).filter(|p| p.is_absolute() && p.is_dir()),
    };
    let picker = state.picker.clone();
    let picked = tokio::task::spawn_blocking(move || picker.pick(start.as_deref()))
        .await
        .map_err(|e| ApiError::internal(format!("folder picker crashed: {e}")))?
        .map_err(ApiError::internal)?;
    let path = picked
        .map(|p| {
            p.into_os_string()
                .into_string()
                .map_err(|p| ApiError::internal(format!("the picked folder's path is not valid UTF-8: {p:?}")))
        })
        .transpose()?;
    Ok(Json(PickFolderView { path }))
}

/// Expands a leading `~/` (or a lone `~`) to `$HOME`.
fn expand_home(dir: &str) -> Result<PathBuf, ApiError> {
    let rest = match dir.strip_prefix('~') {
        None => return Ok(PathBuf::from(dir)),
        Some("") => "",
        Some(r) if r.starts_with('/') => &r[1..],
        Some(_) => return Ok(PathBuf::from(dir)),
    };
    let home = std::env::var_os("HOME")
        .ok_or_else(|| ApiError::bad("cannot expand ~: HOME is not set"))?;
    Ok(PathBuf::from(home).join(rest))
}

async fn post_scan(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ScanReq>,
) -> ApiResult<StateView> {
    let dir = expand_home(req.dir.trim())?;
    let root = dir
        .canonicalize()
        .ok()
        .filter(|p| p.is_dir())
        .ok_or_else(|| ApiError::bad(format!("not a directory: {}", dir.display())))?;
    let session = Session::scan(&root, req.recursive)?;
    let mut inner = state.lock()?;
    session.view(&inner.config)?; // surface plan errors before replacing the session
    inner.session = Some(session);
    inner.generation += 1;
    Ok(Json(state_view(&inner)?))
}

async fn get_plan(State(state): State<Arc<AppState>>) -> ApiResult<PlanView> {
    let inner = state.lock()?;
    Ok(Json(session_ref(&inner)?.view(&inner.config)?))
}

#[derive(Deserialize)]
struct AssignReq {
    path: PathBuf,
    /// `null` = 不搬.
    folder: Option<String>,
}

async fn post_assign(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AssignReq>,
) -> ApiResult<StateView> {
    let mut inner = state.lock()?;
    session_ref(&inner)?;
    let Inner { config, session, .. } = &mut *inner;
    session
        .as_mut()
        .expect("checked above")
        .assign(config, &req.path, req.folder)?;
    Ok(Json(state_view(&inner)?))
}

#[derive(Deserialize)]
struct AssignAllReq {
    paths: Vec<PathBuf>,
    /// `null` = 不搬.
    folder: Option<String>,
}

/// 全部移到…: one destination for every file of a group, all-or-nothing.
async fn post_assign_all(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AssignAllReq>,
) -> ApiResult<StateView> {
    let mut inner = state.lock()?;
    session_ref(&inner)?;
    let Inner { config, session, .. } = &mut *inner;
    session
        .as_mut()
        .expect("checked above")
        .assign_all(config, &req.paths, req.folder)?;
    Ok(Json(state_view(&inner)?))
}

#[derive(Deserialize)]
struct CheckReq {
    paths: Vec<PathBuf>,
    checked: bool,
}

async fn post_check(
    State(state): State<Arc<AppState>>,
    Json(req): Json<CheckReq>,
) -> ApiResult<StateView> {
    let mut inner = state.lock()?;
    session_ref(&inner)?;
    let Inner { config, session, .. } = &mut *inner;
    session
        .as_mut()
        .expect("checked above")
        .set_checked(config, &req.paths, req.checked)?;
    Ok(Json(state_view(&inner)?))
}

#[derive(Deserialize)]
struct NameReq {
    name: String,
}

#[derive(Deserialize)]
struct RenameReq {
    from: String,
    to: String,
}

#[derive(Deserialize)]
struct RememberReq {
    path: PathBuf,
    keyword: String,
}

/// 記住: saves a hand-picked destination as a keyword rule on that folder.
async fn post_remember(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RememberReq>,
) -> ApiResult<StateView> {
    edit_config(&state, |config, session| {
        let session = session.ok_or_else(|| {
            ApiError::new(StatusCode::CONFLICT, "no folder scanned yet; scan one first")
        })?;
        session.remember_keyword(config, &req.path, &req.keyword)?;
        Ok(())
    })
}

async fn post_add_folder(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NameReq>,
) -> ApiResult<StateView> {
    edit_config(&state, |config, _| {
        session::add_folder(config, &req.name)?;
        Ok(())
    })
}

async fn post_rename_folder(
    State(state): State<Arc<AppState>>,
    Json(req): Json<RenameReq>,
) -> ApiResult<StateView> {
    edit_config(&state, |config, session| {
        session::rename_folder(config, session, &req.from, &req.to)?;
        Ok(())
    })
}

async fn post_delete_folder(
    State(state): State<Arc<AppState>>,
    Json(req): Json<NameReq>,
) -> ApiResult<StateView> {
    edit_config(&state, |config, session| {
        session::delete_folder(config, session, &req.name)?;
        Ok(())
    })
}

#[derive(Deserialize)]
struct EmptyReq {}

/// Drops the current scan and everything chosen for it (destinations,
/// checkboxes, AI suggestions). No file is touched; rules stay as they are.
/// Bumps the generation so a pending AI answer is discarded.
async fn post_clear(
    State(state): State<Arc<AppState>>,
    Json(_): Json<EmptyReq>,
) -> ApiResult<StateView> {
    let mut inner = state.lock()?;
    inner.session = None;
    inner.generation += 1;
    Ok(Json(state_view(&inner)?))
}

fn ai_error(e: AiError) -> ApiError {
    match e {
        AiError::BadExclude(_) => ApiError::bad(e),
        _ => ApiError::new(StatusCode::BAD_GATEWAY, e.to_string()),
    }
}

/// Asks the AI about the files no rule classifies (minus `ai_exclude`) and
/// shows the answers as unchecked suggestions. The lock is released while
/// the CLI runs; if the folder was rescanned or the settings changed in
/// the meantime, the answer is discarded with 409.
async fn post_ai_suggest(
    State(state): State<Arc<AppState>>,
    Json(_): Json<EmptyReq>,
) -> ApiResult<StateView> {
    let (generation, config, root, candidates) = {
        let inner = state.lock()?;
        let s = session_ref(&inner)?;
        (
            inner.generation,
            inner.config.clone(),
            s.root().to_path_buf(),
            s.ai_candidates(&inner.config),
        )
    };
    let backend = state.ai.clone();
    let asked = config.clone();
    let suggestions = tokio::task::spawn_blocking(move || {
        let files: Vec<(&FileInfo, u64)> = candidates.iter().map(|(f, n)| (f, *n)).collect();
        ai::suggest(backend.as_ref(), &asked, &root, &files)
    })
    .await
    .map_err(|e| ApiError::internal(format!("AI step crashed: {e}")))?
    .map_err(ai_error)?;

    let mut inner = state.lock()?;
    if inner.generation != generation || inner.config != config {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "the folder was rescanned or the settings changed while the AI was answering; nothing was applied, ask again",
        ));
    }
    let Inner { config, session, .. } = &mut *inner;
    session
        .as_mut()
        .expect("same generation, so the session is still there")
        .set_suggestions(config, suggestions)?;
    Ok(Json(state_view(&inner)?))
}

#[derive(Deserialize)]
struct ApplyReq {
    confirm: bool,
}

#[derive(Serialize)]
struct ApplyResp {
    report: ApplyReport,
    state: StateView,
}

async fn post_apply(
    State(state): State<Arc<AppState>>,
    Json(req): Json<ApplyReq>,
) -> ApiResult<ApplyResp> {
    if !req.confirm {
        return Err(ApiError::bad("apply needs {\"confirm\": true}"));
    }
    let mut inner = state.lock()?;
    let plan = session_ref(&inner)?.apply_plan(&inner.config)?;
    if plan.moves().next().is_none() {
        return Err(ApiError::bad("nothing is checked to move"));
    }
    let now = chrono::Local::now().to_rfc3339();
    let result = apply(&plan, &state.log_path, now);
    // Rescan even when apply failed half-way: some files may have moved.
    let rescanned = session_ref(&inner)?.rescan();
    if let Ok(s) = &rescanned {
        inner.session = Some(s.clone());
        inner.generation += 1;
    }
    let report = result.map_err(ApiError::internal)?;
    rescanned?;
    Ok(Json(ApplyResp {
        report,
        state: state_view(&inner)?,
    }))
}

#[derive(Deserialize)]
struct UndoReq {}

#[derive(Serialize)]
struct UndoResp {
    report: UndoReport,
    state: StateView,
}

async fn post_undo(
    State(state): State<Arc<AppState>>,
    Json(_): Json<UndoReq>,
) -> ApiResult<UndoResp> {
    let mut inner = state.lock()?;
    let report = undo(&state.log_path).map_err(|e| match e {
        MoveError::NoLog(_) | MoveError::NothingToUndo(_) => {
            ApiError::new(StatusCode::CONFLICT, e.to_string())
        }
        _ => ApiError::internal(e),
    })?;
    if let Some(s) = &inner.session {
        let rescanned = s.rescan()?;
        inner.session = Some(rescanned);
        inner.generation += 1;
    }
    Ok(Json(UndoResp {
        report,
        state: state_view(&inner)?,
    }))
}

fn preview_error(e: PreviewError) -> ApiError {
    match e {
        PreviewError::NoThumbnail { .. } | PreviewError::Failed { .. } => {
            ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, e.to_string())
        }
        PreviewError::Timeout { .. } => ApiError::new(StatusCode::GATEWAY_TIMEOUT, e.to_string()),
        PreviewError::NotText(_) => ApiError::new(StatusCode::UNSUPPORTED_MEDIA_TYPE, e.to_string()),
        _ => ApiError::internal(e),
    }
}

/// Previews are only made for files of the current scan, so the API cannot
/// be used to read or render anything else on disk. The scan skips
/// symlinks, but the file (or a folder between it and the scanned folder)
/// may have been replaced by one since; that is refused too, checked right
/// before the preview is made.
fn scanned_file(state: &AppState, path: &Path) -> Result<(), ApiError> {
    let root = {
        let inner = state.lock()?;
        let session = session_ref(&inner)?;
        if !session.has_file(path) {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                format!("{} is not a file of the scanned folder", path.display()),
            ));
        }
        session.root().to_path_buf()
    };
    let rel = path.strip_prefix(&root).map_err(|_| {
        ApiError::internal(format!(
            "scanned file {} is not under the scanned folder {}",
            path.display(),
            root.display()
        ))
    })?;
    let mut at = root;
    for part in rel.components() {
        at.push(part);
        let meta = std::fs::symlink_metadata(&at)
            .map_err(|e| ApiError::internal(format!("cannot read {}: {e}", at.display())))?;
        if meta.file_type().is_symlink() {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                format!("{} is now a symbolic link; it is not previewed (rescan)", at.display()),
            ));
        }
    }
    Ok(())
}

#[derive(Deserialize)]
struct ThumbReq {
    path: PathBuf,
    size: u32,
}

/// A PNG thumbnail of a scanned file. Made locally (Quick Look), never sent
/// anywhere; a file that cannot be rendered is an error, not a placeholder.
async fn get_thumbnail(
    State(state): State<Arc<AppState>>,
    Query(req): Query<ThumbReq>,
) -> Result<Response, ApiError> {
    if req.size == 0 || req.size > MAX_THUMB_SIZE {
        return Err(ApiError::bad(format!("size must be 1..={MAX_THUMB_SIZE}")));
    }
    scanned_file(&state, &req.path)?;
    let backend = state.thumbs.clone();
    let bytes = tokio::task::spawn_blocking(move || backend.thumbnail(&req.path, req.size))
        .await
        .map_err(|e| ApiError::internal(format!("thumbnail step crashed: {e}")))?
        .map_err(preview_error)?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "private, max-age=3600"),
        ],
        bytes,
    )
        .into_response())
}

#[derive(Deserialize)]
struct TextReq {
    path: PathBuf,
}

/// The first lines of a scanned plain-text file.
async fn get_text_preview(
    State(state): State<Arc<AppState>>,
    Query(req): Query<TextReq>,
) -> ApiResult<TextPreview> {
    scanned_file(&state, &req.path)?;
    let text = tokio::task::spawn_blocking(move || preview::text_preview(&req.path))
        .await
        .map_err(|e| ApiError::internal(format!("preview step crashed: {e}")))?
        .map_err(preview_error)?;
    Ok(Json(text))
}

#[derive(Serialize)]
struct FolderCount {
    folder: String,
    count: usize,
}

#[derive(Serialize)]
struct HistoryEntry {
    applied_at: String,
    root: PathBuf,
    count: usize,
    folders: Vec<FolderCount>,
    /// Only the most recent apply can be undone.
    undoable: bool,
}

async fn get_history(State(state): State<Arc<AppState>>) -> ApiResult<Vec<HistoryEntry>> {
    let _inner = state.lock()?;
    let log = MoveLog::load(&state.log_path).map_err(ApiError::internal)?;
    let last = log.applies.len().checked_sub(1);
    let entries = log
        .applies
        .iter()
        .enumerate()
        .rev()
        .map(|(i, batch)| {
            let mut folders: Vec<FolderCount> = Vec::new();
            for m in &batch.moves {
                let folder = m
                    .to
                    .parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                match folders.iter_mut().find(|f| f.folder == folder) {
                    Some(f) => f.count += 1,
                    None => folders.push(FolderCount { folder, count: 1 }),
                }
            }
            HistoryEntry {
                applied_at: batch.applied_at.clone(),
                root: batch.root.clone(),
                count: batch.moves.len(),
                folders,
                undoable: Some(i) == last,
            }
        })
        .collect();
    Ok(Json(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_only_touches_a_leading_tilde_segment() {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(expand_home("~/Desktop").unwrap(), home.join("Desktop"));
        assert_eq!(expand_home("~").unwrap(), home);
        assert_eq!(expand_home("/tmp/x").unwrap(), PathBuf::from("/tmp/x"));
        assert_eq!(expand_home("~other/x").unwrap(), PathBuf::from("~other/x"));
    }
}
