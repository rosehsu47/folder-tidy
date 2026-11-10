//! End-to-end tests of the `folder-tidy scan` CLI on fixture directories only.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn run_scan(dir: &Path, config: &Path, extra: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_folder-tidy"))
        .arg("scan")
        .arg(dir)
        .arg("--config")
        .arg(config)
        .args(extra)
        .output()
        .unwrap()
}

/// Maps file name (relative to `root`) -> category, `None` for unclassified.
fn by_name(json: &Value, root: &Path) -> Vec<(String, Option<String>)> {
    json.as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let path = Path::new(e["path"].as_str().unwrap());
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let cat = match e["status"].as_str().unwrap() {
                "classified" => Some(e["category"].as_str().unwrap().to_owned()),
                "unclassified" => {
                    assert!(
                        e.get("category").is_none(),
                        "unclassified entry has a category: {e}"
                    );
                    None
                }
                other => panic!("unexpected status {other}"),
            };
            (rel, cat)
        })
        .collect()
}

#[test]
fn scan_prints_json_classification_of_mixed_fixture() {
    let root = fixtures().join("mixed");
    let out = run_scan(&root, &fixtures().join("mixed.toml"), &[]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).expect("stdout is JSON");
    assert_eq!(
        by_name(&json, &root),
        vec![
            ("Screenshot-2025.png".into(), Some("Screenshots".into())),
            ("invoice-acme.pdf".into(), Some("Invoices".into())),
            ("mystery.xyz".into(), None),
            ("photo.jpg".into(), Some("Images".into())),
        ]
    );
}

#[test]
fn scan_recursive_includes_subfolders() {
    let root = fixtures().join("mixed");
    let out = run_scan(&root, &fixtures().join("mixed.toml"), &["--recursive"]);
    assert!(out.status.success());
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(by_name(&json, &root).contains(&("sub/deep.jpg".into(), Some("Images".into()))));
}

#[test]
fn scan_classifies_by_modified_date() {
    let dir = tempfile::tempdir().unwrap();
    let old = dir.path().join("old.bin");
    let new = dir.path().join("new.bin");
    fs::write(&old, "o").unwrap();
    fs::write(&new, "n").unwrap();
    let t = std::time::SystemTime::UNIX_EPOCH
        + std::time::Duration::from_secs(1_262_304_000 + 86_400 * 30); // ~2010-01-31
    fs::File::options()
        .write(true)
        .open(&old)
        .unwrap()
        .set_modified(t)
        .unwrap();
    let cfg = dir.path().join("rules.toml");
    // Config lives inside the scanned dir; it is listed too and must be unclassified.
    fs::write(
        &cfg,
        "[[category]]\nname = \"Archive\"\ndate = { before = \"2015-01-01\" }\n",
    )
    .unwrap();

    let out = run_scan(dir.path(), &cfg, &[]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        by_name(&json, dir.path()),
        vec![
            ("new.bin".into(), None),
            ("old.bin".into(), Some("Archive".into())),
            ("rules.toml".into(), None),
        ]
    );
}

#[test]
fn bad_config_fails_loudly() {
    let dir = tempfile::tempdir().unwrap();
    let cfg = dir.path().join("rules.toml");
    fs::write(
        &cfg,
        "[[category]]\nname = \"A\"\nkeywords = [\"x\"]\n[[category]]\nname = \"A\"\n",
    )
    .unwrap();
    let out = run_scan(&fixtures().join("mixed"), &cfg, &[]);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out.stderr).contains("duplicate category name"));
}

#[test]
fn missing_dir_fails_loudly() {
    let out = run_scan(
        &fixtures().join("does-not-exist"),
        &fixtures().join("mixed.toml"),
        &[],
    );
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a directory"));
}
