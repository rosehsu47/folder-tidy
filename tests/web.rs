//! HTTP-level tests of the local web UI's API, on temp-dir fixtures only.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use serde_json::{Value, json};
use tower::ServiceExt;

use folder_tidy::ai::{AiBackend, AiError};
use folder_tidy::config::Config;
use folder_tidy::picker::{FolderPicker, PickError};
use folder_tidy::preview::{PreviewError, ThumbnailBackend};
use folder_tidy::web::{self, AppState};

const RULES: &str = "[[category]]\nname = \"Images\"\nextensions = [\"jpg\"]\n\
                     [[category]]\nname = \"Docs\"\nextensions = [\"pdf\"]\n";

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    config: PathBuf,
    log: PathBuf,
    app: Router,
}

/// Canned AI backend; records every prompt. Tests never run the real CLI.
struct MockAi {
    reply: Result<String, String>,
    prompts: Mutex<Vec<String>>,
}

impl MockAi {
    fn new(reply: Result<&str, &str>) -> Arc<MockAi> {
        Arc::new(MockAi {
            reply: reply.map(String::from).map_err(String::from),
            prompts: Mutex::new(vec![]),
        })
    }
    fn prompts(&self) -> Vec<String> {
        self.prompts.lock().unwrap().clone()
    }
}

impl AiBackend for MockAi {
    fn complete(&self, prompt: &str) -> Result<String, AiError> {
        self.prompts.lock().unwrap().push(prompt.to_owned());
        self.reply.clone().map_err(|stderr| AiError::Failed {
            program: "claude".into(),
            status: "exit status: 1".into(),
            stderr,
        })
    }
}

/// Canned thumbnail backend; records every request. Tests never run Quick Look.
struct MockThumbs {
    fail: bool,
    /// Every call returns `PreviewError::Timeout` (takes precedence over `fail`).
    timeout: bool,
    calls: Mutex<Vec<(PathBuf, u32)>>,
}

const MOCK_PNG: &[u8] = b"\x89PNG\r\n\x1a\nmock";

impl MockThumbs {
    fn new(fail: bool) -> Arc<MockThumbs> {
        Arc::new(MockThumbs {
            fail,
            timeout: false,
            calls: Mutex::new(vec![]),
        })
    }
    fn timing_out() -> Arc<MockThumbs> {
        Arc::new(MockThumbs {
            fail: false,
            timeout: true,
            calls: Mutex::new(vec![]),
        })
    }
    fn calls(&self) -> Vec<(PathBuf, u32)> {
        self.calls.lock().unwrap().clone()
    }
}

impl ThumbnailBackend for MockThumbs {
    fn thumbnail(&self, path: &Path, size: u32) -> Result<Vec<u8>, PreviewError> {
        self.calls.lock().unwrap().push((path.to_path_buf(), size));
        if self.timeout {
            Err(PreviewError::Timeout {
                program: PathBuf::from("mock-qlmanage"),
                path: path.to_path_buf(),
                after: Duration::from_millis(1234),
            })
        } else if self.fail {
            Err(PreviewError::NoThumbnail {
                path: path.to_path_buf(),
                output: "mock cannot render this".into(),
            })
        } else {
            Ok(MOCK_PNG.to_vec())
        }
    }
}

/// Canned folder picker; records every start folder. Tests never open the
/// real dialog: the default fixture's picker fails every call.
struct MockPicker {
    /// `Ok(None)` = Cancel; `Err` = osascript failed with that stderr.
    reply: Result<Option<PathBuf>, String>,
    starts: Mutex<Vec<Option<PathBuf>>>,
}

impl MockPicker {
    fn new(reply: Result<Option<&Path>, &str>) -> Arc<MockPicker> {
        Arc::new(MockPicker {
            reply: reply.map(|p| p.map(Path::to_path_buf)).map_err(String::from),
            starts: Mutex::new(vec![]),
        })
    }
    fn starts(&self) -> Vec<Option<PathBuf>> {
        self.starts.lock().unwrap().clone()
    }
}

impl FolderPicker for MockPicker {
    fn pick(&self, start: Option<&Path>) -> Result<Option<PathBuf>, PickError> {
        self.starts.lock().unwrap().push(start.map(Path::to_path_buf));
        self.reply.clone().map_err(|stderr| PickError::Failed {
            status: "exit status: 1".into(),
            stderr,
        })
    }
}

/// `<tmp>/root` holds a.jpg, b.pdf, c.xyz; config and log live outside it.
/// The AI backend refuses every call.
fn fixture() -> Fixture {
    fixture_with(MockAi::new(Err("no AI in this test")), RULES)
}

fn fixture_with(ai: Arc<dyn AiBackend>, rules: &str) -> Fixture {
    fixture_full(ai, MockThumbs::new(true), rules)
}

fn fixture_full(ai: Arc<dyn AiBackend>, thumbs: Arc<dyn ThumbnailBackend>, rules: &str) -> Fixture {
    fixture_all(ai, thumbs, MockPicker::new(Err("no folder picker in this test")), rules)
}

fn fixture_all(
    ai: Arc<dyn AiBackend>,
    thumbs: Arc<dyn ThumbnailBackend>,
    picker: Arc<dyn FolderPicker>,
    rules: &str,
) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let root = base.join("root");
    fs::create_dir(&root).unwrap();
    for name in ["a.jpg", "b.pdf", "c.xyz"] {
        fs::write(root.join(name), name).unwrap();
    }
    let config = base.join("rules.toml");
    fs::write(&config, rules).unwrap();
    let log = base.join("moves.json");
    let app = web::router(AppState::with_all(&config, &log, ai, thumbs, picker).unwrap());
    Fixture {
        _dir: dir,
        root,
        config,
        log,
        app,
    }
}

async fn send(app: &Router, method: &str, uri: &str, host: &str, body: Option<Value>) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, host);
    let body = match body {
        Some(v) => {
            req = req.header(header::CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

async fn call(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let (status, text) = send(app, method, uri, "127.0.0.1:8737", body).await;
    let json = serde_json::from_str(&text).unwrap_or_else(|e| panic!("not JSON ({e}): {text}"));
    (status, json)
}

async fn ok(app: &Router, method: &str, uri: &str, body: Option<Value>) -> Value {
    let (status, json) = call(app, method, uri, body).await;
    assert_eq!(status, StatusCode::OK, "{method} {uri}: {json}");
    json
}

async fn scan(f: &Fixture) -> Value {
    ok(&f.app, "POST", "/api/scan", Some(json!({ "dir": f.root, "recursive": false }))).await
}

fn group<'a>(plan: &'a Value, folder: Option<&str>, kind: &str) -> &'a Value {
    plan["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["kind"] == kind && g["folder"].as_str() == folder)
        .unwrap_or_else(|| panic!("no group {kind} {folder:?} in {plan}"))
}

fn names(group: &Value) -> Vec<String> {
    group["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap().to_owned())
        .collect()
}

fn file<'a>(plan: &'a Value, name: &str) -> &'a Value {
    plan["groups"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|g| g["files"].as_array().unwrap())
        .find(|f| f["name"] == name)
        .unwrap_or_else(|| panic!("no file {name} in {plan}"))
}

fn tree(root: &Path) -> Vec<String> {
    let mut out: Vec<String> = walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .map(|e| {
            let e = e.unwrap();
            e.path().strip_prefix(root).unwrap().to_string_lossy().into_owned()
        })
        .collect();
    out.sort();
    out
}

#[tokio::test]
async fn scan_plan_apply_and_undo() {
    let f = fixture();
    scan(&f).await;

    let plan = ok(&f.app, "GET", "/api/plan", None).await;
    assert_eq!(plan["total"], 3);
    let images = group(&plan, Some("Images"), "folder");
    assert_eq!(names(images), ["a.jpg"]);
    assert_eq!(images["target"], json!(f.root.join("Images")));
    assert_eq!(names(group(&plan, Some("Docs"), "folder")), ["b.pdf"]);
    assert_eq!(names(group(&plan, None, "none")), ["c.xyz"]);
    assert_eq!(file(&plan, "a.jpg")["checked"], true);

    // Uncheck b.pdf: only checked files move.
    let b = f.root.join("b.pdf");
    ok(&f.app, "POST", "/api/check", Some(json!({ "paths": [b], "checked": false }))).await;

    // Without confirmation nothing moves.
    let (status, _) = call(&f.app, "POST", "/api/apply", Some(json!({ "confirm": false }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(tree(&f.root), ["a.jpg", "b.pdf", "c.xyz"]);

    let resp = ok(&f.app, "POST", "/api/apply", Some(json!({ "confirm": true }))).await;
    assert_eq!(resp["report"]["moved"].as_array().unwrap().len(), 1);
    assert_eq!(tree(&f.root), ["Images", "Images/a.jpg", "b.pdf", "c.xyz"]);
    assert!(f.log.exists());
    let history = ok(&f.app, "GET", "/api/history", None).await;
    assert_eq!(history[0]["count"], 1);
    assert_eq!(history[0]["folders"], json!([{ "folder": "Images", "count": 1 }]));
    assert_eq!(history[0]["undoable"], true);

    let resp = ok(&f.app, "POST", "/api/undo", Some(json!({}))).await;
    assert_eq!(resp["report"]["restored"].as_array().unwrap().len(), 1);
    assert_eq!(tree(&f.root), ["a.jpg", "b.pdf", "c.xyz"]);
    // The session was rescanned: a.jpg is planned again from its original place.
    assert_eq!(file(&resp["state"]["plan"], "a.jpg")["path"], json!(f.root.join("a.jpg")));
    assert_eq!(ok(&f.app, "GET", "/api/history", None).await, json!([]));
}

#[tokio::test]
async fn assign_all_moves_a_whole_group_or_nothing() {
    // 全部移到…: merge the Images group into Docs, then send it all to 不搬.
    let f = fixture();
    scan(&f).await;
    let a = f.root.join("a.jpg");
    let c = f.root.join("c.xyz");
    let state = ok(&f.app, "POST", "/api/assign-all", Some(json!({ "paths": [a, c], "folder": "Docs" }))).await;
    let plan = &state["plan"];
    for name in ["a.jpg", "c.xyz", "b.pdf"] {
        assert_eq!(file(plan, name)["dest"], json!("Docs"), "{name}");
    }
    assert_eq!(file(plan, "a.jpg")["checked"], json!(true));
    assert_eq!(file(plan, "c.xyz")["source"], json!("manual"));

    // One bad path or an unknown folder: error, and nothing changes.
    let before = ok(&f.app, "GET", "/api/plan", None).await;
    let (status, _) = call(&f.app, "POST", "/api/assign-all", Some(json!({ "paths": [a, f.root.join("nope.txt")], "folder": null }))).await;
    assert!(status.is_client_error(), "{status}");
    let (status, _) = call(&f.app, "POST", "/api/assign-all", Some(json!({ "paths": [a], "folder": "Nope" }))).await;
    assert!(status.is_client_error(), "{status}");
    assert_eq!(ok(&f.app, "GET", "/api/plan", None).await, before);

    let state = ok(&f.app, "POST", "/api/assign-all", Some(json!({ "paths": [a, c], "folder": null }))).await;
    assert_eq!(file(&state["plan"], "a.jpg")["dest"], json!(null));
    assert_eq!(file(&state["plan"], "a.jpg")["checked"], json!(false));
}

#[tokio::test]
async fn index_page_has_group_select_all_and_move_all() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    for needle in ["data-change=\"checkGroup\"", "data-change=\"moveGroup\"", "全部移到…", "/api/assign-all", "indeterminate"] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}

#[tokio::test]
async fn pick_folder_returns_the_picked_path_and_starts_in_the_typed_folder() {
    let picked = PathBuf::from("/Users/someone/Desktop/截圖");
    let picker = MockPicker::new(Ok(Some(&picked)));
    let f = fixture_all(MockAi::new(Err("no AI")), MockThumbs::new(true), picker.clone(), RULES);

    let j = ok(&f.app, "POST", "/api/pick-folder", Some(json!({ "start": f.root }))).await;
    assert_eq!(j, json!({ "path": "/Users/someone/Desktop/截圖" }));
    // Empty, missing, relative or nonexistent start folders open the dialog's default.
    let missing = f.root.join("nope");
    for body in [json!({ "start": "" }), json!({}), json!({ "start": "root" }), json!({ "start": missing })] {
        ok(&f.app, "POST", "/api/pick-folder", Some(body)).await;
    }
    assert_eq!(picker.starts(), vec![Some(f.root.clone()), None, None, None, None]);
}

#[tokio::test]
async fn pick_folder_cancel_is_null_and_a_failure_is_an_error() {
    let f = fixture_all(MockAi::new(Err("no AI")), MockThumbs::new(true), MockPicker::new(Ok(None)), RULES);
    let j = ok(&f.app, "POST", "/api/pick-folder", Some(json!({ "start": "" }))).await;
    assert_eq!(j, json!({ "path": null }));

    let f = fixture_all(MockAi::new(Err("no AI")), MockThumbs::new(true), MockPicker::new(Err("boom")), RULES);
    let (status, j) = call(&f.app, "POST", "/api/pick-folder", Some(json!({ "start": "" }))).await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let msg = j["error"].as_str().unwrap();
    assert!(msg.contains("boom") && msg.contains("type the path instead"), "{msg}");
}

#[tokio::test]
async fn clear_drops_the_scan_and_touches_no_file() {
    let f = fixture();
    scan(&f).await;
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": f.root.join("c.xyz"), "folder": "Docs" }))).await;
    let rules_before = fs::read_to_string(&f.config).unwrap();

    let j = ok(&f.app, "POST", "/api/clear", Some(json!({}))).await;
    assert!(j["plan"].is_null(), "{j}");
    assert_eq!(fs::read_to_string(&f.config).unwrap(), rules_before);
    for name in ["a.jpg", "b.pdf", "c.xyz"] {
        assert!(f.root.join(name).is_file(), "{name} moved");
    }
    let (status, _) = call(&f.app, "GET", "/api/plan", None).await;
    assert_eq!(status, StatusCode::CONFLICT);
    // A new scan starts fresh: the earlier hand-picked destination is gone.
    let plan = scan(&f).await;
    assert_eq!(file(&plan["plan"], "c.xyz")["dest"], Value::Null);
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    assert!(html.contains("data-act=\"clearPlan\"") && html.contains("清除預覽"));
}

#[tokio::test]
async fn index_page_names_new_folders_from_fresh_server_state() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    for needle in ["function addNamed(", "api('GET', '/api/state')", "addNamed('新分類'", "addNamed('新資料夾'"] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
    assert!(!html.contains("var name = uniqueName("), "an add still names folders from the page's own copy");
}

#[tokio::test]
async fn index_page_has_folder_picker_button() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    for needle in ["data-act=\"pickDir\"", "選擇…", "/api/pick-folder"] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}

#[tokio::test]
async fn create_rename_and_delete_folder() {
    let f = fixture();
    scan(&f).await;
    let c = f.root.join("c.xyz");

    ok(&f.app, "POST", "/api/folders", Some(json!({ "name": "Misc" }))).await;
    assert!(Config::load(&f.config).unwrap().category("Misc").is_some());
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": c, "folder": "Misc" }))).await;

    // Rename: the stored folder (= rule target) changes, and files follow.
    let state = ok(
        &f.app,
        "POST",
        "/api/folders/rename",
        Some(json!({ "from": "Misc", "to": "Stuff" })),
    )
    .await;
    let saved = Config::load(&f.config).unwrap();
    assert!(saved.category("Misc").is_none());
    assert!(saved.category("Stuff").is_some());
    let plan = &state["plan"];
    assert_eq!(group(plan, Some("Stuff"), "folder")["target"], json!(f.root.join("Stuff")));
    assert_eq!(file(plan, "c.xyz")["to"], json!(f.root.join("Stuff/c.xyz")));

    // Renaming a folder that has rules keeps its rules under the new name.
    let state = ok(
        &f.app,
        "POST",
        "/api/folders/rename",
        Some(json!({ "from": "Images", "to": "Photos" })),
    )
    .await;
    assert_eq!(
        Config::load(&f.config).unwrap().category("Photos").unwrap().extensions,
        ["jpg"]
    );
    assert_eq!(file(&state["plan"], "a.jpg")["to"], json!(f.root.join("Photos/a.jpg")));

    // Bad and duplicate names are refused.
    for to in ["Docs", "a/../b", "../x", "/abs", "a//b", "a\\b", ".."] {
        let (status, body) = call(
            &f.app,
            "POST",
            "/api/folders/rename",
            Some(json!({ "from": "Stuff", "to": to })),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{to}: {body}");
    }

    // Delete: its files become 不搬, nothing on disk is deleted.
    let before = tree(&f.root);
    let state = ok(&f.app, "POST", "/api/folders/delete", Some(json!({ "name": "Stuff" }))).await;
    let c_view = file(&state["plan"], "c.xyz");
    assert_eq!(c_view["dest"], Value::Null);
    assert_eq!(c_view["checked"], false);
    assert!(Config::load(&f.config).unwrap().category("Stuff").is_none());

    // Deleting a non-empty rule folder: its files become 不搬 too.
    let state = ok(&f.app, "POST", "/api/folders/delete", Some(json!({ "name": "Photos" }))).await;
    assert_eq!(file(&state["plan"], "a.jpg")["dest"], Value::Null);
    assert_eq!(names(group(&state["plan"], None, "none")), ["a.jpg", "c.xyz"]);
    assert_eq!(tree(&f.root), before);
}

#[tokio::test]
async fn nested_folder_names_are_accepted_and_escaping_ones_move_nothing() {
    let f = fixture();
    scan(&f).await;
    let before = tree(&f.root);

    // Add a nested folder, assign a file, rename to another nested name.
    ok(&f.app, "POST", "/api/folders", Some(json!({ "name": "工作/專案A" }))).await;
    assert!(Config::load(&f.config).unwrap().category("工作/專案A").is_some());
    let c = f.root.join("c.xyz");
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": c, "folder": "工作/專案A" }))).await;
    let state = ok(
        &f.app,
        "POST",
        "/api/folders/rename",
        Some(json!({ "from": "工作/專案A", "to": "工作/報表" })),
    )
    .await;
    assert_eq!(file(&state["plan"], "c.xyz")["to"], json!(f.root.join("工作/報表/c.xyz")));

    // Renaming to ../x is a 400; the config and the disk stay as they were.
    let (status, body) = call(
        &f.app,
        "POST",
        "/api/folders/rename",
        Some(json!({ "from": "工作/報表", "to": "../x" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(Config::load(&f.config).unwrap().category("工作/報表").is_some());
    assert_eq!(tree(&f.root), before);
    assert!(!f.root.parent().unwrap().join("x").exists());

    // Applying creates both levels; undo removes only those.
    ok(&f.app, "POST", "/api/check", Some(json!({ "paths": [c], "checked": true }))).await;
    ok(&f.app, "POST", "/api/apply", Some(json!({ "confirm": true }))).await;
    assert_eq!(fs::read_to_string(f.root.join("工作/報表/c.xyz")).unwrap(), "c.xyz");
    ok(&f.app, "POST", "/api/undo", Some(json!({}))).await;
    assert_eq!(tree(&f.root), before);
}

#[tokio::test]
async fn reassigning_a_file_marks_it_manual() {
    let f = fixture();
    scan(&f).await;
    let a = f.root.join("a.jpg");

    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": a, "folder": "Docs" }))).await;
    let plan = ok(&f.app, "GET", "/api/plan", None).await;
    assert_eq!(names(group(&plan, Some("Docs"), "folder")), ["a.jpg", "b.pdf"]);
    let a_view = file(&plan, "a.jpg");
    assert_eq!(a_view["source"], "manual");
    assert_eq!(a_view["checked"], true);
    assert_eq!(a_view["to"], json!(f.root.join("Docs/a.jpg")));

    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": a, "folder": null }))).await;
    let plan = ok(&f.app, "GET", "/api/plan", None).await;
    assert_eq!(names(group(&plan, None, "none")), ["a.jpg", "c.xyz"]);
    let a_view = file(&plan, "a.jpg");
    assert_eq!(a_view["source"], "manual");
    assert_eq!(a_view["dest"], Value::Null);
    assert_eq!(a_view["checked"], false);

    // Unknown folders and files are errors, not silently ignored.
    let (status, _) = call(&f.app, "POST", "/api/assign", Some(json!({ "path": a, "folder": "Nope" }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &f.app,
        "POST",
        "/api/assign",
        Some(json!({ "path": f.root.join("missing.jpg"), "folder": "Docs" })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn config_edits_persist_and_invalid_ones_are_refused() {
    let f = fixture();
    let mut cfg = ok(&f.app, "GET", "/api/config", None).await;
    cfg["ai_exclude"] = json!(["~/Documents/tax", "*.key"]);
    cfg["category"][1]["keywords"] = json!(["invoice"]);
    cfg["category"][1]["date"] = json!({ "before": "2024-01-01" });
    ok(&f.app, "PUT", "/api/config", Some(cfg)).await;
    let saved = Config::load(&f.config).unwrap();
    assert_eq!(saved.ai_exclude, ["~/Documents/tax", "*.key"]);
    assert_eq!(saved.category("Docs").unwrap().keywords, ["invoice"]);
    assert!(saved.category("Docs").unwrap().date.is_some());

    let on_disk = fs::read_to_string(&f.config).unwrap();
    let bad = json!({ "category": [{ "name": "A", "date": { "after": "2025-01-01", "before": "2024-01-01" } }] });
    let (status, _) = call(&f.app, "PUT", "/api/config", Some(bad)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&f.app, "PUT", "/api/config", Some(json!({ "category": [{ "name": "../x" }] }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(fs::read_to_string(&f.config).unwrap(), on_disk);
}

#[tokio::test]
async fn relative_date_preset_persists_and_mixing_is_refused() {
    // T-042
    let f = fixture();
    let mut cfg = ok(&f.app, "GET", "/api/config", None).await;
    cfg["category"][1]["date"] = json!({ "newer_than_days": 30 });
    ok(&f.app, "PUT", "/api/config", Some(cfg.clone())).await;
    let saved = Config::load(&f.config).unwrap();
    assert_eq!(saved.category("Docs").unwrap().date.as_ref().unwrap().newer_than_days, Some(30));
    assert!(fs::read_to_string(&f.config).unwrap().contains("newer_than_days = 30"));

    let on_disk = fs::read_to_string(&f.config).unwrap();
    cfg["category"][1]["date"] = json!({ "newer_than_days": 30, "after": "2024-01-01" });
    let (status, body) = call(&f.app, "PUT", "/api/config", Some(cfg)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body.to_string().contains("mixes absolute dates"), "{body}");
    assert_eq!(fs::read_to_string(&f.config).unwrap(), on_disk);
}

#[tokio::test]
async fn plan_before_scan_and_undo_without_log_fail_loudly() {
    let f = fixture();
    let (status, body) = call(&f.app, "GET", "/api/plan", None).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = call(&f.app, "POST", "/api/undo", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, _) = call(&f.app, "POST", "/api/scan", Some(json!({ "dir": f.root.join("nope") }))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn index_page_has_nav_animation_and_reduced_motion() {
    let f = fixture();
    let (status, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    assert_eq!(status, StatusCode::OK);
    for needle in ["來源", "預覽", "分類設定", "紀錄", "@keyframes fly", "prefers-reduced-motion", "之後提供"] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}

#[tokio::test]
async fn foreign_host_and_non_json_posts_are_refused() {
    let f = fixture();
    scan(&f).await;
    let (status, _) = send(&f.app, "GET", "/api/plan", "evil.example:8737", None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = send(&f.app, "GET", "/", "localhost:8737", None).await;
    assert_eq!(status, StatusCode::OK);

    // A form-style POST (no JSON content type) cannot trigger an apply.
    let req = Request::builder()
        .method("POST")
        .uri("/api/apply")
        .header(header::HOST, "127.0.0.1:8737")
        .header(header::CONTENT_TYPE, "text/plain")
        .body(Body::from(r#"{"confirm":true}"#))
        .unwrap();
    let resp = f.app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(tree(&f.root), ["a.jpg", "b.pdf", "c.xyz"]);
}

#[tokio::test]
async fn server_binds_loopback_only() {
    assert_eq!(web::BIND_IP, std::net::Ipv4Addr::new(127, 0, 0, 1));
    let listener = web::bind(0).await.unwrap();
    let addr = listener.local_addr().unwrap();
    assert_eq!(addr.ip(), std::net::IpAddr::from([127, 0, 0, 1]));
    assert_ne!(addr.port(), 0);
}

// ---------- AI suggestions (mock backend only) ----------

const AI_SAYS_DOCS: &str = r#"{"suggestions":[{"id":1,"folder":"Docs","reason":"像是文件"}]}"#;

#[tokio::test]
async fn ai_suggestions_are_shown_unchecked_and_only_move_once_checked() {
    let ai = MockAi::new(Ok(AI_SAYS_DOCS));
    let f = fixture_with(ai.clone(), RULES);
    scan(&f).await;

    let state = ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let plan = &state["plan"];
    assert_eq!(names(group(plan, None, "ai")), ["c.xyz"]);
    let c = file(plan, "c.xyz");
    assert_eq!(
        (&c["source"], &c["dest"], &c["checked"], &c["reason"]),
        (&json!("ai"), &json!("Docs"), &json!(false), &json!("像是文件"))
    );
    assert!(plan["groups"].as_array().unwrap().iter().all(|g| g["kind"] != "none"));
    assert_eq!(plan["moving"], 2, "the suggestion is not counted as a move");

    // Confirmed apply without checking it: the suggested file stays put.
    ok(&f.app, "POST", "/api/apply", Some(json!({ "confirm": true }))).await;
    assert_eq!(tree(&f.root), ["Docs", "Docs/b.pdf", "Images", "Images/a.jpg", "c.xyz"]);

    // Ask again, check it, confirm: now it moves.
    ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let c = f.root.join("c.xyz");
    ok(&f.app, "POST", "/api/check", Some(json!({ "paths": [c], "checked": true }))).await;
    ok(&f.app, "POST", "/api/apply", Some(json!({ "confirm": true }))).await;
    assert!(f.root.join("Docs/c.xyz").exists());
    assert_eq!(ai.prompts().len(), 2);
}

/// Gives a different canned answer on each call, in order.
struct SeqAi(Mutex<Vec<&'static str>>);

impl AiBackend for SeqAi {
    fn complete(&self, _prompt: &str) -> Result<String, AiError> {
        Ok(self.0.lock().unwrap().remove(0).to_owned())
    }
}

#[tokio::test]
async fn re_asking_with_a_different_answer_drops_the_old_checkmark() {
    let rules = format!("{RULES}[[category]]\nname = \"Other\"\nextensions = []\n");
    let ai = Arc::new(SeqAi(Mutex::new(vec![
        AI_SAYS_DOCS,
        r#"{"suggestions":[{"id":1,"folder":"Other","reason":"別的"}]}"#,
    ])));
    let f = fixture_with(ai, &rules);
    fs::write(f.root.join("d.xyz"), "d").unwrap();
    scan(&f).await;
    // A manual file and a rule file, both unchecked by the user.
    let a = f.root.join("a.jpg");
    let d = f.root.join("d.xyz");
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": d, "folder": "Docs" }))).await;
    ok(&f.app, "POST", "/api/check", Some(json!({ "paths": [a, d], "checked": false }))).await;

    // Suggestion A (Docs), checked by the user.
    ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let c = f.root.join("c.xyz");
    let state = ok(&f.app, "POST", "/api/check", Some(json!({ "paths": [c], "checked": true }))).await;
    assert_eq!(file(&state["plan"], "c.xyz")["checked"], true);

    // Re-ask: the AI now says Other. The file must start unchecked again.
    let state = ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let plan = &state["plan"];
    let cv = file(plan, "c.xyz");
    assert_eq!(
        (&cv["source"], &cv["dest"], &cv["checked"]),
        (&json!("ai"), &json!("Other"), &json!(false))
    );
    // Rule and manual files keep the user's unchecked state.
    assert_eq!((&file(plan, "a.jpg")["source"], &file(plan, "a.jpg")["checked"]), (&json!("rule"), &json!(false)));
    assert_eq!((&file(plan, "d.xyz")["source"], &file(plan, "d.xyz")["checked"]), (&json!("manual"), &json!(false)));

    // A confirmed apply moves only b.pdf; c.xyz goes nowhere.
    ok(&f.app, "POST", "/api/apply", Some(json!({ "confirm": true }))).await;
    assert_eq!(tree(&f.root), ["Docs", "Docs/b.pdf", "a.jpg", "c.xyz", "d.xyz"]);
}

#[tokio::test]
async fn re_asking_with_the_same_answer_keeps_the_checkmark() {
    let ai = Arc::new(SeqAi(Mutex::new(vec![AI_SAYS_DOCS, AI_SAYS_DOCS])));
    let f = fixture_with(ai, RULES);
    scan(&f).await;
    ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let c = f.root.join("c.xyz");
    ok(&f.app, "POST", "/api/check", Some(json!({ "paths": [c], "checked": true }))).await;
    let state = ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let cv = file(&state["plan"], "c.xyz");
    assert_eq!((&cv["dest"], &cv["checked"]), (&json!("Docs"), &json!(true)));
}

#[tokio::test]
async fn only_unclassified_non_excluded_files_reach_the_ai() {
    let rules = format!("ai_exclude = [\"tax\", \"*.key\"]\n{RULES}");
    let ai = MockAi::new(Ok(r#"{"suggestions":[]}"#));
    let f = fixture_with(ai.clone(), &rules);
    fs::create_dir(f.root.join("tax")).unwrap();
    fs::write(f.root.join("tax/return-2025.xyz"), "x").unwrap();
    fs::write(f.root.join("wallet.KEY"), "x").unwrap();
    ok(&f.app, "POST", "/api/scan", Some(json!({ "dir": f.root, "recursive": true }))).await;

    ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    let prompts = ai.prompts();
    assert_eq!(prompts.len(), 1);
    let p = &prompts[0];
    assert!(p.contains("c.xyz"), "{p}");
    for never in ["return-2025", "wallet", "a.jpg", "b.pdf"] {
        assert!(!p.contains(never), "{never} was sent to the AI:\n{p}");
    }

    // A file the user placed by hand is not asked about either.
    let c = f.root.join("c.xyz");
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": c, "folder": "Docs" }))).await;
    ok(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    assert_eq!(ai.prompts().len(), 1, "nothing left to ask, so no call");
}

#[tokio::test]
async fn ai_failure_or_bad_answer_is_an_error_and_changes_nothing() {
    for (ai, needle) in [
        (MockAi::new(Err("Not logged in")), "Not logged in"),
        (MockAi::new(Ok("I think c.xyz is a document.")), "unusable answer"),
        (
            MockAi::new(Ok(r#"{"suggestions":[{"id":1,"folder":"Nowhere","reason":"x"}]}"#)),
            "not one of the folders",
        ),
    ] {
        let f = fixture_with(ai, RULES);
        scan(&f).await;
        let (status, body) = call(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
        assert!(body["error"].as_str().unwrap().contains(needle), "{body}");
        let plan = ok(&f.app, "GET", "/api/plan", None).await;
        assert_eq!(names(group(&plan, None, "none")), ["c.xyz"]);
        assert_eq!(file(&plan, "c.xyz")["source"], "none");
    }
}

#[tokio::test]
async fn ai_before_scan_fails_loudly() {
    let f = fixture_with(MockAi::new(Ok(AI_SAYS_DOCS)), RULES);
    let (status, body) = call(&f.app, "POST", "/api/ai/suggest", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

/// Signals when the AI call starts, then blocks until the test releases
/// it, so a test can change server state while the AI is "answering".
struct BlockingAi {
    entered: Mutex<mpsc::Sender<()>>,
    release: Mutex<mpsc::Receiver<()>>,
    calls: Mutex<usize>,
}

impl AiBackend for BlockingAi {
    fn complete(&self, _prompt: &str) -> Result<String, AiError> {
        *self.calls.lock().unwrap() += 1;
        self.entered.lock().unwrap().send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .expect("the test never released the AI call");
        Ok(AI_SAYS_DOCS.to_owned())
    }
}

/// Scans, starts /api/ai/suggest with a [`BlockingAi`] and returns once
/// the AI call is in progress (the state lock is released by then).
async fn start_blocked_suggest() -> (
    Fixture,
    Arc<BlockingAi>,
    tokio::task::JoinHandle<(StatusCode, Value)>,
    mpsc::Sender<()>,
) {
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let ai = Arc::new(BlockingAi {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
        calls: Mutex::new(0),
    });
    let f = fixture_with(ai.clone(), RULES);
    scan(&f).await;
    let app = f.app.clone();
    let pending =
        tokio::spawn(async move { call(&app, "POST", "/api/ai/suggest", Some(json!({}))).await });
    tokio::task::spawn_blocking(move || entered_rx.recv_timeout(Duration::from_secs(10)))
        .await
        .unwrap()
        .expect("the AI call never started");
    (f, ai, pending, release_tx)
}

/// Releases the blocked AI call and asserts its answer was thrown away.
async fn assert_discarded(
    f: &Fixture,
    ai: &BlockingAi,
    pending: tokio::task::JoinHandle<(StatusCode, Value)>,
    release: mpsc::Sender<()>,
) {
    release.send(()).unwrap();
    let (status, body) = pending.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body["error"].as_str().unwrap().contains("nothing was applied"), "{body}");
    assert_eq!(*ai.calls.lock().unwrap(), 1, "the 409 must come after the AI answered");

    let plan = ok(&f.app, "GET", "/api/plan", None).await;
    for g in plan["groups"].as_array().unwrap() {
        assert_ne!(g["kind"], "ai", "{plan}");
        for file in g["files"].as_array().unwrap() {
            assert_ne!(file["source"], "ai", "{plan}");
        }
    }
    assert_eq!(file(&plan, "c.xyz")["source"], "none");
}

#[tokio::test]
async fn ai_answer_is_discarded_when_the_folder_is_rescanned_meanwhile() {
    let (f, ai, pending, release) = start_blocked_suggest().await;
    // Same folder, same files: only the scan generation tells them apart.
    scan(&f).await;
    assert_discarded(&f, &ai, pending, release).await;
}

#[tokio::test]
async fn ai_answer_is_discarded_when_the_settings_change_meanwhile() {
    let (f, ai, pending, release) = start_blocked_suggest().await;
    let mut cfg = ok(&f.app, "GET", "/api/config", None).await;
    cfg["category"][1]["keywords"] = json!(["invoice"]);
    ok(&f.app, "PUT", "/api/config", Some(cfg)).await;
    assert_discarded(&f, &ai, pending, release).await;
    assert_eq!(Config::load(&f.config).unwrap().category("Docs").unwrap().keywords, ["invoice"]);
}

#[tokio::test]
async fn keyword_can_be_switched_off_and_on_via_the_config() {
    let f = fixture();
    let mut cfg = ok(&f.app, "GET", "/api/config", None).await;
    cfg["category"][1]["keywords"] = json!(["invoice"]);
    ok(&f.app, "PUT", "/api/config", Some(cfg)).await;

    // Switch off: the keyword moves to disabled_keywords and is kept on disk.
    let mut cfg = ok(&f.app, "GET", "/api/config", None).await;
    cfg["category"][1]["keywords"] = json!([]);
    cfg["category"][1]["disabled_keywords"] = json!(["invoice"]);
    ok(&f.app, "PUT", "/api/config", Some(cfg)).await;
    let saved = Config::load(&f.config).unwrap();
    let docs = saved.category("Docs").unwrap();
    assert!(docs.keywords.is_empty());
    assert_eq!(docs.disabled_keywords, ["invoice"]);
    assert!(fs::read_to_string(&f.config).unwrap().contains("disabled_keywords"));
    let cfg = ok(&f.app, "GET", "/api/config", None).await;
    assert_eq!(cfg["category"][1]["disabled_keywords"], json!(["invoice"]));

    // Switch back on.
    let mut cfg = cfg;
    cfg["category"][1]["keywords"] = json!(["invoice"]);
    cfg["category"][1]["disabled_keywords"] = json!([]);
    ok(&f.app, "PUT", "/api/config", Some(cfg)).await;
    let saved = Config::load(&f.config).unwrap();
    assert_eq!(saved.category("Docs").unwrap().keywords, ["invoice"]);
    assert!(saved.category("Docs").unwrap().disabled_keywords.is_empty());
}

#[tokio::test]
async fn remembering_a_manual_choice_saves_a_keyword_rule_for_later_scans() {
    let f = fixture();
    fs::write(f.root.join("Invoice-2026.xyz"), "x").unwrap();
    fs::write(f.root.join("invoice-jan.xyz"), "x").unwrap();
    scan(&f).await;
    let first = f.root.join("Invoice-2026.xyz");
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": first, "folder": "Docs" }))).await;

    // Case-insensitive containment; the keyword is trimmed.
    let state = ok(&f.app, "POST", "/api/rules/remember", Some(json!({ "path": first, "keyword": " invoice " }))).await;
    assert_eq!(state["config"]["category"][1]["keywords"], json!(["invoice"]));
    let saved = Config::load(&f.config).unwrap();
    assert_eq!(saved.category("Docs").unwrap().keywords, ["invoice"]);

    // Remembering it again does not list it twice.
    ok(&f.app, "POST", "/api/rules/remember", Some(json!({ "path": first, "keyword": "INVOICE" }))).await;
    assert_eq!(Config::load(&f.config).unwrap().category("Docs").unwrap().keywords, ["invoice"]);

    // A rescan classifies the other file by rule.
    let state = scan(&f).await;
    let other = file(&state["plan"], "invoice-jan.xyz");
    assert_eq!((other["source"].as_str(), other["dest"].as_str()), (Some("rule"), Some("Docs")));
    assert_eq!(other["reason"], "keyword: invoice");
}

#[tokio::test]
async fn remembering_refuses_bad_requests_and_changes_nothing() {
    let f = fixture();
    let c = f.root.join("c.xyz");
    let before = fs::read_to_string(&f.config).unwrap();

    // Before any scan.
    let (status, _) = call(&f.app, "POST", "/api/rules/remember", Some(json!({ "path": c, "keyword": "c" }))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    scan(&f).await;
    let remember = |path: PathBuf, keyword: &str| {
        let app = f.app.clone();
        let keyword = keyword.to_owned();
        async move { call(&app, "POST", "/api/rules/remember", Some(json!({ "path": path, "keyword": keyword }))).await }
    };
    // No folder picked by hand (rule or none).
    assert_eq!(remember(c.clone(), "c").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(remember(f.root.join("a.jpg"), "a").await.0, StatusCode::BAD_REQUEST);
    // Picked 不搬 by hand: no folder either.
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": c, "folder": null }))).await;
    assert_eq!(remember(c.clone(), "c").await.0, StatusCode::BAD_REQUEST);

    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": c, "folder": "Docs" }))).await;
    assert_eq!(remember(c.clone(), "").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(remember(c.clone(), "   ").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(remember(c.clone(), "zzz").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(remember(f.root.join("missing.xyz"), "m").await.0, StatusCode::BAD_REQUEST);
    assert_eq!(fs::read_to_string(&f.config).unwrap(), before);
}

#[tokio::test]
async fn remembering_a_switched_off_keyword_switches_it_back_on() {
    let f = fixture_with(
        MockAi::new(Err("no AI in this test")),
        "[[category]]\nname = \"Docs\"\ndisabled_keywords = [\"c\"]\n",
    );
    scan(&f).await;
    let c = f.root.join("c.xyz");
    ok(&f.app, "POST", "/api/assign", Some(json!({ "path": c, "folder": "Docs" }))).await;
    ok(&f.app, "POST", "/api/rules/remember", Some(json!({ "path": c, "keyword": "C" }))).await;
    let docs = Config::load(&f.config).unwrap().category("Docs").unwrap().clone();
    assert_eq!(docs.keywords, ["C"]);
    assert!(docs.disabled_keywords.is_empty());
}

#[tokio::test]
async fn index_page_has_the_remember_control() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    assert!(html.contains("data-act=\"remStart\"") && html.contains("/api/rules/remember"));
}

#[tokio::test]
async fn index_page_has_the_keyword_toggle() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    assert!(html.contains("data-act=\"toggleKw\"") && html.contains("disabled_keywords"));
}

#[tokio::test]
async fn index_page_has_the_ai_button() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    assert!(html.contains("data-act=\"aiSuggest\"") && html.contains("/api/ai/suggest"));
}

#[tokio::test]
async fn index_page_caps_concurrent_thumbnail_requests() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    // Hung Quick Look runs must not take all 6 browser connections per host.
    let cap = html
        .split("var MAX_THUMB_FETCHES = ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .and_then(|n| n.trim().parse::<u32>().ok())
        .expect("index.html declares var MAX_THUMB_FETCHES = <n>;");
    assert!((1..=4).contains(&cap), "MAX_THUMB_FETCHES is {cap}, must be 1..=4");
    // List thumbnails no longer load as plain <img src> straight from the row.
    assert!(!html.contains("loading=\"lazy\" data-thumb="), "a row still loads its thumbnail directly");
}

#[tokio::test]
async fn index_page_names_result_tiles_by_category_not_parent_folder() {
    // T-045: tiles after 確認搬移 use the folder relative to the scanned root
    // (the category name, nested or not), not just the last path segment.
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    assert!(html.contains("function destCategory(to, root)"), "index.html lacks destCategory");
    assert!(html.contains("dest: destCategory(m.to, root)"), "tiles are not named by destCategory");
    assert!(!html.contains("parentName(m.to)"), "tiles still use parentName");
}

#[tokio::test]
async fn index_page_offers_relative_date_presets() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    for needle in ["一週內", "一個月內", "超過一年", "newer_than_days", "older_than_days", "data-change=\"datePreset\""] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}

#[tokio::test]
async fn index_page_returns_focus_to_the_rename_button() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    // T-030: committing or cancelling a folder rename puts focus back on that
    // folder's rename button (restored after render, like pvReturn).
    assert!(html.contains("renameReturn"), "index.html lacks renameReturn");
    assert!(html.contains("[data-act=\"startEdit\"]"), "render() does not look up the rename button");
}

#[tokio::test]
async fn index_page_says_ai_is_thinking_only_during_an_ai_request() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    // T-029: the label follows aiBusy (set only around /api/ai/suggest), not
    // the generic busy flag every request sets.
    assert!(html.contains("aiBusy"), "index.html lacks aiBusy");
    assert!(html.contains("(S.aiBusy ? 'AI 判斷中…'"), "the AI label does not follow S.aiBusy");
    assert!(!html.contains("(S.busy ? 'AI 判斷中…'"), "the AI label still follows S.busy");
}

#[tokio::test]
async fn index_page_queues_thumbnails_only_near_the_viewport() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    // T-035: rows queue their thumbnail when the placeholder nears the
    // viewport, not all at once when the list is built.
    for needle in ["IntersectionObserver", "data-thumbwait", "data-thumburl", "rootMargin"] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}

// ---------- previews (mock thumbnail backend only) ----------

fn preview_fixture(thumbs: Arc<MockThumbs>) -> Fixture {
    fixture_full(MockAi::new(Err("no AI in this test")), thumbs, RULES)
}

/// Percent-encodes a path for a query string.
fn q(path: &Path) -> String {
    path.to_string_lossy()
        .bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// GET returning status, content type and raw body bytes.
async fn get_raw(app: &Router, uri: &str) -> (StatusCode, String, Vec<u8>) {
    let req = Request::builder()
        .uri(uri)
        .header(header::HOST, "127.0.0.1:8737")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ctype = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.to_str().unwrap().to_owned())
        .unwrap_or_default();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    (status, ctype, bytes.to_vec())
}

#[tokio::test]
async fn thumbnail_of_a_scanned_file_is_image_bytes_from_the_backend() {
    let thumbs = MockThumbs::new(false);
    let f = preview_fixture(thumbs.clone());
    let plan = scan(&f).await["plan"].clone();
    assert_eq!(file(&plan, "a.jpg")["preview"], "image");
    assert_eq!(file(&plan, "b.pdf")["preview"], "pdf");
    assert_eq!(file(&plan, "c.xyz")["preview"], "other");

    let a = f.root.join("a.jpg");
    let (status, ctype, body) = get_raw(&f.app, &format!("/api/thumbnail?path={}&size=96&v=x", q(&a))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctype, "image/png");
    assert_eq!(body, MOCK_PNG);
    assert_eq!(thumbs.calls(), [(a, 96)]);
}

#[tokio::test]
async fn failing_thumbnail_backend_is_an_explicit_error() {
    let f = preview_fixture(MockThumbs::new(true));
    scan(&f).await;
    let a = f.root.join("a.jpg");
    let (status, ctype, body) = get_raw(&f.app, &format!("/api/thumbnail?path={}&size=96", q(&a))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(ctype.starts_with("application/json"), "{ctype}");
    let err: Value = serde_json::from_slice(&body).unwrap();
    assert!(err["error"].as_str().unwrap().contains("mock cannot render this"), "{err}");
}

#[tokio::test]
async fn timed_out_thumbnail_is_a_504_with_the_timeout_message() {
    let thumbs = MockThumbs::timing_out();
    let f = preview_fixture(thumbs.clone());
    scan(&f).await;
    let a = f.root.join("a.jpg");
    let (status, ctype, body) = get_raw(&f.app, &format!("/api/thumbnail?path={}&size=96", q(&a))).await;
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert!(ctype.starts_with("application/json"), "{ctype}");
    let err: Value = serde_json::from_slice(&body).unwrap();
    let msg = err["error"].as_str().unwrap();
    assert!(msg.contains("mock-qlmanage took longer than 1234 ms and was stopped"), "{err}");
    assert_eq!(thumbs.calls(), [(a, 96)]);
}

#[tokio::test]
async fn text_preview_returns_the_first_lines() {
    let f = preview_fixture(MockThumbs::new(true));
    let body: String = (1..=40).map(|i| format!("待辦 {i}\n")).collect();
    fs::write(f.root.join("notes.txt"), body).unwrap();
    fs::write(f.root.join("blob.txt"), b"\0\x01\x02binary").unwrap();
    let plan = scan(&f).await["plan"].clone();
    assert_eq!(file(&plan, "notes.txt")["preview"], "text");

    let uri = format!("/api/preview/text?path={}", q(&f.root.join("notes.txt")));
    let t = ok(&f.app, "GET", &uri, None).await;
    let lines = t["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 20);
    assert_eq!(lines[0], "待辦 1");
    assert_eq!(lines[19], "待辦 20");
    assert_eq!(t["truncated"], true);

    let uri = format!("/api/preview/text?path={}", q(&f.root.join("blob.txt")));
    let (status, body) = call(&f.app, "GET", &uri, None).await;
    assert_eq!(status, StatusCode::UNSUPPORTED_MEDIA_TYPE, "{body}");
}

#[tokio::test]
async fn previews_outside_the_scanned_folder_are_rejected() {
    let thumbs = MockThumbs::new(false);
    let f = preview_fixture(thumbs.clone());

    // Before any scan there is nothing to preview.
    let uri = format!("/api/thumbnail?path={}&size=96", q(&f.root.join("a.jpg")));
    assert_eq!(call(&f.app, "GET", &uri, None).await.0, StatusCode::CONFLICT);

    scan(&f).await;
    // The rules file sits next to the scanned folder, not inside it.
    let outside = [f.config.clone(), f.root.join("../rules.toml"), PathBuf::from("/etc/hosts")];
    for p in &outside {
        let (status, body) = call(&f.app, "GET", &format!("/api/thumbnail?path={}&size=96", q(p)), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}: {body}", p.display());
        let (status, body) = call(&f.app, "GET", &format!("/api/preview/text?path={}", q(p)), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}: {body}", p.display());
    }
    assert!(thumbs.calls().is_empty(), "backend must not run for rejected paths");

    // Size limits.
    let a = q(&f.root.join("a.jpg"));
    for size in ["0", "5000"] {
        let (status, _) = call(&f.app, "GET", &format!("/api/thumbnail?path={a}&size={size}"), None).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "size {size}");
    }
}

#[tokio::test]
async fn a_scanned_file_replaced_by_a_symlink_is_not_previewed() {
    use std::os::unix::fs::symlink;
    let thumbs = MockThumbs::new(false);
    let f = preview_fixture(thumbs.clone());
    fs::create_dir(f.root.join("sub")).unwrap();
    fs::write(f.root.join("sub/n.txt"), "inside").unwrap();
    ok(&f.app, "POST", "/api/scan", Some(json!({ "dir": f.root, "recursive": true }))).await;

    // After the scan: a.jpg becomes a link to the rules file outside the
    // folder, and the folder `sub` becomes a link to a look-alike elsewhere.
    let a = f.root.join("a.jpg");
    fs::remove_file(&a).unwrap();
    symlink(&f.config, &a).unwrap();
    let elsewhere = f.root.join("../elsewhere");
    fs::create_dir(&elsewhere).unwrap();
    fs::write(elsewhere.join("n.txt"), "outside").unwrap();
    fs::rename(f.root.join("sub"), f.root.join("../sub-moved")).unwrap();
    symlink(&elsewhere, f.root.join("sub")).unwrap();

    for p in [a, f.root.join("sub/n.txt")] {
        let (status, body) = call(&f.app, "GET", &format!("/api/thumbnail?path={}&size=96", q(&p)), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}: {body}", p.display());
        assert!(body["error"].as_str().unwrap().contains("symbolic link"), "{body}");
        let (status, body) = call(&f.app, "GET", &format!("/api/preview/text?path={}", q(&p)), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{}: {body}", p.display());
        assert!(body["error"].as_str().unwrap().contains("symbolic link"), "{body}");
    }
    assert!(thumbs.calls().is_empty(), "backend must not run for a symlink");

    // An untouched scanned file still previews.
    let (status, _, _) = get_raw(&f.app, &format!("/api/thumbnail?path={}&size=96", q(&f.root.join("b.pdf")))).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn index_page_has_the_preview_drawer() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    for needle in [
        "role=\"dialog\"",
        "data-act=\"pvStep\" data-dir=\"-1\"",
        "data-act=\"pvStep\" data-dir=\"1\"",
        "上一個",
        "下一個",
        "data-act=\"openPv\"",
        "/api/thumbnail",
        "/api/preview/text",
        "無法預覽",
        "ArrowLeft",
        "Escape",
    ] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}

#[tokio::test]
async fn index_page_keeps_focus_in_the_preview_drawer() {
    let f = fixture();
    let (_, html) = send(&f.app, "GET", "/", "127.0.0.1:8737", None).await;
    for needle in [
        // Tab / Shift+Tab wrap inside the open drawer.
        "e.key === 'Tab' && S.screen === 'preview' && pvFile()",
        "function drawerFocusables()",
        "e.shiftKey && document.activeElement === first",
        "!e.shiftKey && document.activeElement === last",
        // Closing returns focus to the row of the file last shown.
        "S.pvReturn = S.pv",
        ".namebtn[data-act=\"openPv\"]",
    ] {
        assert!(html.contains(needle), "index.html lacks {needle:?}");
    }
}
