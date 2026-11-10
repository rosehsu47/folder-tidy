//! End-to-end tests of `folder-tidy plan` (dry run), `apply` and `undo`.
//! Apply/undo only ever run on temp-dir copies of the fixtures.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn run(args: &[&std::ffi::OsStr]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_folder-tidy"))
        .args(args)
        .output()
        .unwrap()
}

fn plan(dir: &Path, config: &Path) -> Output {
    run(&["plan".as_ref(), dir.as_ref(), "--config".as_ref(), config.as_ref()])
}

fn apply(dir: &Path, config: &Path, log: &Path, yes: bool) -> Output {
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "apply".as_ref(),
        dir.as_ref(),
        "--config".as_ref(),
        config.as_ref(),
        "--log".as_ref(),
        log.as_ref(),
    ];
    if yes {
        args.push("--yes".as_ref());
    }
    run(&args)
}

fn undo(log: &Path) -> Output {
    run(&["undo".as_ref(), "--log".as_ref(), log.as_ref()])
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Every path under `root` (relative) -> file bytes, `None` for folders.
fn snapshot(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    walkdir::WalkDir::new(root)
        .min_depth(1)
        .into_iter()
        .map(|e| {
            let e = e.unwrap();
            let rel = e.path().strip_prefix(root).unwrap().to_string_lossy().into_owned();
            let bytes = e.file_type().is_file().then(|| fs::read(e.path()).unwrap());
            (rel, bytes)
        })
        .collect()
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in walkdir::WalkDir::new(from).min_depth(1) {
        let e = e.unwrap();
        let dest = to.join(e.path().strip_prefix(from).unwrap());
        if e.file_type().is_dir() {
            fs::create_dir_all(&dest).unwrap();
        } else {
            fs::copy(e.path(), &dest).unwrap();
        }
    }
}

/// A temp copy of the `mixed` fixture, plus a log path outside it.
struct Workspace {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    log: PathBuf,
}

fn workspace() -> Workspace {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("mixed");
    copy_dir(&fixtures().join("mixed"), &root);
    let log = tmp.path().join("moves.json");
    Workspace {
        _tmp: tmp,
        root,
        log,
    }
}

fn config() -> PathBuf {
    fixtures().join("mixed.toml")
}

#[test]
fn dry_run_prints_plan_and_leaves_fixture_byte_identical() {
    let root = fixtures().join("mixed");
    let before = snapshot(&root);
    let out = plan(&root, &config());
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    assert_eq!(snapshot(&root), before, "dry run changed the fixture");

    let json: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    let root = root.canonicalize().unwrap();
    let rel = |p: &Value| {
        Path::new(p.as_str().unwrap())
            .strip_prefix(&root)
            .unwrap()
            .to_string_lossy()
            .into_owned()
    };
    let entries: Vec<(String, String, Option<String>)> = json["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                rel(&e["from"]),
                e["status"].as_str().unwrap().to_owned(),
                e.get("to").map(rel),
            )
        })
        .collect();
    assert_eq!(
        entries,
        vec![
            ("Screenshot-2025.png".into(), "move".into(), Some("Screenshots/Screenshot-2025.png".into())),
            ("invoice-acme.pdf".into(), "move".into(), Some("Invoices/invoice-acme.pdf".into())),
            ("mystery.xyz".into(), "unclassified".into(), None),
            ("photo.jpg".into(), "move".into(), Some("Images/photo.jpg".into())),
        ]
    );
}

#[test]
fn apply_without_yes_moves_nothing_and_fails() {
    let ws = workspace();
    let before = snapshot(&ws.root);
    let out = apply(&ws.root, &config(), &ws.log, false);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("--yes"), "stderr: {}", stderr(&out));
    assert_eq!(snapshot(&ws.root), before);
    assert!(!ws.log.exists());
}

#[test]
fn apply_moves_files_into_category_folders_and_undo_restores_them() {
    let ws = workspace();
    let before = snapshot(&ws.root);

    let out = apply(&ws.root, &config(), &ws.log, true);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["moved"].as_array().unwrap().len(), 3);
    assert!(report["collisions"].as_array().unwrap().is_empty());

    let r = &ws.root;
    assert_eq!(fs::read(r.join("Images/photo.jpg")).unwrap(), before["photo.jpg"].clone().unwrap());
    assert!(r.join("Screenshots/Screenshot-2025.png").is_file());
    assert!(r.join("Invoices/invoice-acme.pdf").is_file());
    assert!(!r.join("photo.jpg").exists());
    assert!(!r.join("Screenshot-2025.png").exists());
    assert!(!r.join("invoice-acme.pdf").exists());
    assert!(r.join("mystery.xyz").is_file(), "unclassified file stays");
    assert!(r.join("sub/deep.jpg").is_file(), "non-recursive apply leaves subfolders");

    let out = undo(&ws.log);
    assert!(out.status.success(), "stderr: {}", stderr(&out));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["restored"].as_array().unwrap().len(), 3);
    assert_eq!(snapshot(&ws.root), before, "undo restores the original layout exactly");

    let out = undo(&ws.log);
    assert!(!out.status.success());
    assert!(stderr(&out).contains("nothing to undo"), "stderr: {}", stderr(&out));
}

#[test]
fn apply_does_not_overwrite_existing_destination_and_reports_collision() {
    let ws = workspace();
    fs::create_dir(ws.root.join("Images")).unwrap();
    fs::write(ws.root.join("Images/photo.jpg"), "already here").unwrap();
    let original = fs::read(ws.root.join("photo.jpg")).unwrap();

    let out = apply(&ws.root, &config(), &ws.log, true);
    assert_eq!(out.status.code(), Some(2), "stderr: {}", stderr(&out));
    assert!(stderr(&out).contains("not moved because the destination already exists"));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let collisions = report["collisions"].as_array().unwrap();
    assert_eq!(collisions.len(), 1);
    assert!(collisions[0]["from"].as_str().unwrap().ends_with("/photo.jpg"));
    assert!(collisions[0]["to"].as_str().unwrap().ends_with("/Images/photo.jpg"));
    assert_eq!(collisions[0]["reason"], "destination already exists");

    assert_eq!(fs::read_to_string(ws.root.join("Images/photo.jpg")).unwrap(), "already here");
    assert_eq!(fs::read(ws.root.join("photo.jpg")).unwrap(), original);
    // The other two files still moved.
    assert_eq!(report["moved"].as_array().unwrap().len(), 2);
    assert!(ws.root.join("Invoices/invoice-acme.pdf").is_file());

    // The plan shows the collision too.
    let out = plan(&ws.root, &config());
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(json["entries"].as_array().unwrap().iter().any(|e| e["status"] == "collision"
        && e["from"].as_str().unwrap().ends_with("/photo.jpg")));
}

#[test]
fn undo_without_log_fails_loudly() {
    let tmp = tempfile::tempdir().unwrap();
    let out = undo(&tmp.path().join("missing.json"));
    assert!(!out.status.success());
    assert!(stderr(&out).contains("no move log"), "stderr: {}", stderr(&out));
}
