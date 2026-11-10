//! Real Quick Look check. It runs `qlmanage` from PATH (macOS only), so it is
//! `#[ignore]`d and headless rounds skip it; run it by hand with
//! `cargo test --test preview -- --ignored`.

use std::path::Path;

use folder_tidy::preview::{QuickLook, ThumbnailBackend};

#[test]
#[ignore = "runs the real qlmanage (macOS Quick Look)"]
fn real_qlmanage_makes_a_png_of_the_fixture_image() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/preview/sample.svg");
    let cache = tempfile::tempdir().unwrap();
    let ql = QuickLook {
        cache_dir: cache.path().to_path_buf(),
        ..QuickLook::default()
    };
    let png = ql.thumbnail(&fixture, 128).unwrap_or_else(|e| panic!("{e}"));
    assert!(png.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG: {:?}", &png[..png.len().min(16)]);
    // Second call is served from the cache with the same bytes.
    assert_eq!(ql.thumbnail(&fixture, 128).unwrap(), png);
}
