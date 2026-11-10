//! File previews for the web UI's 預覽 screen: thumbnails and text excerpts.
//!
//! - Thumbnails come from macOS Quick Look (`qlmanage -t`), run as a
//!   subprocess into a private temp dir and cached on disk, so no image
//!   crate is needed. The backend sits behind [`ThumbnailBackend`] so tests
//!   use a mock and never depend on Quick Look.
//! - A Quick Look run is killed after [`QuickLook::timeout`], at most
//!   [`RunSlots`] runs happen at once, and the cache is trimmed (oldest
//!   files first) to [`QuickLook::cache_max_bytes`].
//! - Plain-text files are previewed by reading their first lines
//!   ([`text_preview`]); binary content is refused, not shown as garbage.
//! - Everything is generated locally. Nothing here sends a preview, a
//!   thumbnail or file contents anywhere.
//! - Failures are errors: there is no placeholder image. The caller decides
//!   how to show "無法預覽".

use std::collections::hash_map::DefaultHasher;
use std::fmt;
use std::fs;
use std::hash::{Hash, Hasher};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// Largest thumbnail edge (pixels) a caller may ask for.
pub const MAX_THUMB_SIZE: u32 = 1024;
/// Lines returned by [`text_preview`].
pub const TEXT_LINES: usize = 20;
/// Bytes read by [`text_preview`].
pub const TEXT_BYTES: usize = 8 * 1024;

/// Produces a PNG thumbnail of `path` that fits in `size`×`size` pixels.
pub trait ThumbnailBackend: Send + Sync {
    fn thumbnail(&self, path: &Path, size: u32) -> Result<Vec<u8>, PreviewError>;
}

#[derive(Debug)]
pub enum PreviewError {
    /// The thumbnail program could not be found.
    NotFound(PathBuf),
    Spawn { program: PathBuf, source: std::io::Error },
    /// The thumbnail program exited non-zero.
    Failed { program: PathBuf, status: String, output: String },
    /// The program ran but made no thumbnail (Quick Look cannot render it).
    NoThumbnail { path: PathBuf, output: String },
    /// The program ran longer than the timeout and was killed.
    Timeout { program: PathBuf, path: PathBuf, after: Duration },
    Io { path: PathBuf, source: std::io::Error },
    /// The file is not UTF-8 text.
    NotText(PathBuf),
}

impl fmt::Display for PreviewError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PreviewError::NotFound(p) => write!(f, "cannot make a thumbnail: {} not found", p.display()),
            PreviewError::Spawn { program, source } => {
                write!(f, "cannot make a thumbnail: cannot run {}: {source}", program.display())
            }
            PreviewError::Failed {
                program,
                status,
                output,
            } => write!(
                f,
                "cannot make a thumbnail: {} exited with {status}: {}",
                program.display(),
                output.trim()
            ),
            PreviewError::NoThumbnail { path, output } => write!(
                f,
                "Quick Look cannot render {}{}",
                path.display(),
                if output.trim().is_empty() {
                    String::new()
                } else {
                    format!(": {}", output.trim())
                }
            ),
            PreviewError::Timeout { program, path, after } => write!(
                f,
                "cannot make a thumbnail of {}: {} took longer than {} ms and was stopped",
                path.display(),
                program.display(),
                after.as_millis()
            ),
            PreviewError::Io { path, source } => write!(f, "cannot read {}: {source}", path.display()),
            PreviewError::NotText(p) => write!(f, "{} is not plain text", p.display()),
        }
    }
}

impl std::error::Error for PreviewError {}

fn io_err(path: &Path) -> impl FnOnce(std::io::Error) -> PreviewError + '_ {
    move |source| PreviewError::Io {
        path: path.to_path_buf(),
        source,
    }
}

const IMAGE_EXT: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "heic", "heif", "webp", "tif", "tiff", "bmp", "svg",
];
const VIDEO_EXT: &[&str] = &["mov", "mp4", "m4v", "avi", "mkv", "webm"];
const TEXT_EXT: &[&str] = &[
    "txt", "md", "markdown", "csv", "tsv", "log", "json", "toml", "yaml", "yml", "xml", "ini",
    "conf", "html", "htm", "css", "js", "ts", "rs", "py", "sh",
];

/// How the UI should preview a file, by extension: `image`, `pdf` and
/// `video` get a Quick Look thumbnail in the list, `text` shows its first
/// lines, `other` is tried with Quick Look in the drawer only.
pub fn kind(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let ext = ext.as_str();
    if IMAGE_EXT.contains(&ext) {
        "image"
    } else if ext == "pdf" {
        "pdf"
    } else if VIDEO_EXT.contains(&ext) {
        "video"
    } else if TEXT_EXT.contains(&ext) {
        "text"
    } else {
        "other"
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TextPreview {
    pub lines: Vec<String>,
    /// More of the file exists than was returned.
    pub truncated: bool,
}

/// The first [`TEXT_LINES`] lines of a UTF-8 text file (reading at most
/// [`TEXT_BYTES`]). A file with NUL bytes or invalid UTF-8 is
/// [`PreviewError::NotText`].
pub fn text_preview(path: &Path) -> Result<TextPreview, PreviewError> {
    let file = fs::File::open(path).map_err(io_err(path))?;
    let mut buf = Vec::with_capacity(TEXT_BYTES + 1);
    file.take(TEXT_BYTES as u64 + 1)
        .read_to_end(&mut buf)
        .map_err(io_err(path))?;
    let mut truncated = buf.len() > TEXT_BYTES;
    buf.truncate(TEXT_BYTES);
    if buf.contains(&0) {
        return Err(PreviewError::NotText(path.to_path_buf()));
    }
    let text = match std::str::from_utf8(&buf) {
        Ok(t) => t,
        // A multi-byte character cut by the read limit is fine; anything
        // else is not text.
        Err(e) if truncated && e.error_len().is_none() => {
            std::str::from_utf8(&buf[..e.valid_up_to()]).expect("valid prefix")
        }
        Err(_) => return Err(PreviewError::NotText(path.to_path_buf())),
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    if lines.len() > TEXT_LINES {
        lines.truncate(TEXT_LINES);
        truncated = true;
    }
    Ok(TextPreview { lines, truncated })
}

/// Default [`QuickLook::timeout`].
pub const QL_TIMEOUT: Duration = Duration::from_secs(20);
/// Default number of Quick Look runs allowed at once.
pub const QL_MAX_RUNNING: usize = 4;
/// Default [`QuickLook::cache_max_bytes`] (200 MiB).
pub const CACHE_MAX_BYTES: u64 = 200 * 1024 * 1024;
/// How often a running Quick Look child is checked for exit.
const POLL: Duration = Duration::from_millis(20);

/// Caps how many thumbnail runs happen at once. Callers past the cap wait
/// (in their blocking thread) until a run finishes. Shared by every clone of
/// a [`QuickLook`].
#[derive(Debug)]
pub struct RunSlots {
    max: usize,
    running: Mutex<usize>,
    freed: Condvar,
}

impl RunSlots {
    /// `max` must be at least 1.
    pub fn new(max: usize) -> Arc<RunSlots> {
        assert!(max >= 1, "RunSlots needs at least one slot");
        Arc::new(RunSlots {
            max,
            running: Mutex::new(0),
            freed: Condvar::new(),
        })
    }

    fn acquire(&self) -> SlotGuard<'_> {
        // The lock only guards a counter and nothing in these critical
        // sections can panic, so poisoning would be a bug: fail loudly.
        let mut running = self.running.lock().expect("thumbnail slot counter poisoned");
        while *running >= self.max {
            running = self.freed.wait(running).expect("thumbnail slot counter poisoned");
        }
        *running += 1;
        SlotGuard(self)
    }
}

struct SlotGuard<'a>(&'a RunSlots);

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut running = self.0.running.lock().expect("thumbnail slot counter poisoned");
        *running -= 1;
        self.0.freed.notify_one();
    }
}

/// macOS Quick Look via `qlmanage -t -s <size> -o <dir> <file>`, cached in
/// `cache_dir` by (path, size on disk, modified time, thumbnail size).
#[derive(Debug, Clone)]
pub struct QuickLook {
    pub program: PathBuf,
    pub cache_dir: PathBuf,
    /// A run taking longer than this is killed: [`PreviewError::Timeout`].
    pub timeout: Duration,
    /// Limits concurrent runs (shared by clones).
    pub slots: Arc<RunSlots>,
    /// After each new thumbnail, the oldest cached thumbnails are deleted
    /// until the cache holds at most this many bytes ([`trim_cache`]).
    pub cache_max_bytes: u64,
}

impl Default for QuickLook {
    fn default() -> Self {
        QuickLook {
            program: PathBuf::from("qlmanage"),
            cache_dir: std::env::temp_dir().join("folder-tidy-thumbnails"),
            timeout: QL_TIMEOUT,
            slots: RunSlots::new(QL_MAX_RUNNING),
            cache_max_bytes: CACHE_MAX_BYTES,
        }
    }
}

/// A run dir untouched for this long belongs to no live run (each run is
/// killed at [`QL_TIMEOUT`]); [`trim_cache`] removes it (D-009).
pub const STALE_RUN_AGE: Duration = Duration::from_secs(QL_TIMEOUT.as_secs() * 3);

/// Removes `run-*` dirs directly in `dir` whose mtime is older than
/// [`STALE_RUN_AGE`]: ones a straggling qlmanage recreated after cleanup, a
/// killed server left, or that hold a subdirectory `remove_run_dir` skips.
/// `remove_dir_all` removes links inside without following them. A
/// `run-*` entry that is not a real directory (a link, a `.png.partial`
/// file) is not a run dir and is left alone. Younger dirs may be live runs.
fn prune_stale_run_dirs(dir: &Path) -> Result<(), PreviewError> {
    let now = SystemTime::now();
    for entry in fs::read_dir(dir).map_err(io_err(dir))? {
        let entry = entry.map_err(io_err(dir))?;
        let path = entry.path();
        if !entry.file_name().to_string_lossy().starts_with("run-") {
            continue;
        }
        // DirEntry::file_type does not follow symlinks.
        match entry.file_type() {
            Ok(t) if t.is_dir() => {}
            Ok(_) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&path)(e)),
        }
        let modified = match entry.metadata().and_then(|m| m.modified()) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&path)(e)),
        };
        // A clock set back makes the age negative: treat as fresh, not stale.
        if now.duration_since(modified).unwrap_or_default() < STALE_RUN_AGE {
            continue;
        }
        match fs::remove_dir_all(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
    }
    Ok(())
}

/// Deletes cached thumbnails (`*.png` directly in `dir`), oldest modified
/// first, until their total size is at most `max_bytes`, and removes stale
/// run dirs ([`prune_stale_run_dirs`]). Live run dirs and `*.png.partial`
/// files are left alone. A file another trim removed first is not an error.
pub fn trim_cache(dir: &Path, max_bytes: u64) -> Result<(), PreviewError> {
    prune_stale_run_dirs(dir)?;
    let mut files: Vec<(SystemTime, u64, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(io_err(dir))? {
        let entry = entry.map_err(io_err(dir))?;
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "png") {
            continue;
        }
        let meta = match entry.metadata() {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io_err(&path)(e)),
        };
        if !meta.is_file() {
            continue;
        }
        let modified = meta.modified().map_err(io_err(&path))?;
        files.push((modified, meta.len(), path));
    }
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    files.sort();
    for (_, len, path) in files {
        if total <= max_bytes {
            break;
        }
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(io_err(&path)(e)),
        }
        total -= len;
    }
    Ok(())
}

/// Removes one run's private dir: first the entries directly in it (files,
/// or links themselves, never followed), then the dir. Not recursive: a
/// subdirectory inside it is left, and then removing the dir fails.
fn remove_run_dir(run_dir: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(run_dir)? {
        let entry = entry?;
        // DirEntry::file_type does not follow symlinks.
        if entry.file_type()?.is_dir() {
            continue;
        }
        match fs::remove_file(entry.path()) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    fs::remove_dir(run_dir)
}

static RUN_ID: AtomicU64 = AtomicU64::new(0);

impl QuickLook {
    /// Arguments for one thumbnail run.
    pub fn args(path: &Path, size: u32, out_dir: &Path) -> Vec<std::ffi::OsString> {
        vec![
            "-t".into(),
            "-s".into(),
            size.to_string().into(),
            "-o".into(),
            out_dir.as_os_str().to_owned(),
            path.as_os_str().to_owned(),
        ]
    }

    fn cache_path(&self, path: &Path, size: u32) -> Result<PathBuf, PreviewError> {
        let meta = fs::metadata(path).map_err(io_err(path))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let mut h = DefaultHasher::new();
        (path, meta.len(), modified, size).hash(&mut h);
        Ok(self.cache_dir.join(format!("{:016x}-{size}.png", h.finish())))
    }

    fn generate(&self, path: &Path, size: u32, out_dir: &Path) -> Result<Vec<u8>, PreviewError> {
        // stdout and stderr go to a file, not pipes: nothing has to drain
        // them while we wait, and a grandchild that outlives a killed
        // qlmanage cannot keep a pipe (and us) open.
        let said_path = out_dir.join("qlmanage.out");
        let said_file = fs::File::create(&said_path).map_err(io_err(&said_path))?;
        let said_err = said_file.try_clone().map_err(io_err(&said_path))?;
        let mut child = Command::new(&self.program)
            .args(QuickLook::args(path, size, out_dir))
            .stdin(Stdio::null())
            .stdout(said_file)
            .stderr(said_err)
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => PreviewError::NotFound(self.program.clone()),
                _ => PreviewError::Spawn {
                    program: self.program.clone(),
                    source: e,
                },
            })?;
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(io_err(&self.program))? {
                break status;
            }
            if started.elapsed() >= self.timeout {
                child.kill().map_err(io_err(&self.program))?;
                child.wait().map_err(io_err(&self.program))?;
                fs::remove_file(&said_path).map_err(io_err(&said_path))?;
                return Err(PreviewError::Timeout {
                    program: self.program.clone(),
                    path: path.to_path_buf(),
                    after: self.timeout,
                });
            }
            std::thread::sleep(POLL);
        };
        let said = String::from_utf8_lossy(&fs::read(&said_path).map_err(io_err(&said_path))?).into_owned();
        fs::remove_file(&said_path).map_err(io_err(&said_path))?;
        if !status.success() {
            return Err(PreviewError::Failed {
                program: self.program.clone(),
                status: status.to_string(),
                output: said,
            });
        }
        // One input file, so the only .png in our private dir is its thumbnail.
        let png = fs::read_dir(out_dir)
            .map_err(io_err(out_dir))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| p.extension().is_some_and(|e| e == "png"));
        match png {
            Some(p) => {
                let bytes = fs::read(&p).map_err(io_err(&p))?;
                fs::remove_file(&p).map_err(io_err(&p))?;
                Ok(bytes)
            }
            None => Err(PreviewError::NoThumbnail {
                path: path.to_path_buf(),
                output: said,
            }),
        }
    }
}

impl ThumbnailBackend for QuickLook {
    fn thumbnail(&self, path: &Path, size: u32) -> Result<Vec<u8>, PreviewError> {
        let cached = self.cache_path(path, size)?;
        if let Ok(bytes) = fs::read(&cached) {
            return Ok(bytes);
        }
        let run_dir = self.cache_dir.join(format!(
            "run-{}-{}",
            std::process::id(),
            RUN_ID.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&run_dir).map_err(io_err(&run_dir))?;
        let result = {
            let _slot = self.slots.acquire();
            self.generate(path, size, &run_dir)
        };
        // Only our own run dir, including anything a failed or timed-out
        // run left in it (a thumbnail written before the kill, other
        // output). A failure here is not worth failing a thumbnail that
        // was made.
        let _ = remove_run_dir(&run_dir);
        let bytes = result?;
        // Write then rename, so a concurrent reader never sees half a file.
        let partial = run_dir.with_extension("png.partial");
        fs::write(&partial, &bytes).map_err(io_err(&partial))?;
        fs::rename(&partial, &cached).map_err(io_err(&cached))?;
        trim_cache(&self.cache_dir, self.cache_max_bytes)?;
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_by_extension() {
        assert_eq!(kind(Path::new("a/IMG_1.HEIC")), "image");
        assert_eq!(kind(Path::new("x.pdf")), "pdf");
        assert_eq!(kind(Path::new("clip.mov")), "video");
        assert_eq!(kind(Path::new("notes.md")), "text");
        assert_eq!(kind(Path::new("disk.dmg")), "other");
        assert_eq!(kind(Path::new("README")), "other");
    }

    #[test]
    fn text_preview_returns_first_lines() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.txt");
        let body: String = (1..=30).map(|i| format!("第 {i} 行\n")).collect();
        fs::write(&p, format!("\u{feff}{body}")).unwrap();
        let t = text_preview(&p).unwrap();
        assert_eq!(t.lines.len(), TEXT_LINES);
        assert_eq!(t.lines[0], "第 1 行");
        assert!(t.truncated);

        fs::write(&p, "one\ntwo").unwrap();
        assert_eq!(
            text_preview(&p).unwrap(),
            TextPreview {
                lines: vec!["one".into(), "two".into()],
                truncated: false
            }
        );
    }

    #[test]
    fn text_preview_cut_inside_a_character_is_still_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.txt");
        // 3-byte characters, so the 8 KiB limit splits one.
        fs::write(&p, "中".repeat(TEXT_BYTES)).unwrap();
        let t = text_preview(&p).unwrap();
        assert!(t.truncated);
        assert_eq!(t.lines[0].chars().count(), TEXT_BYTES / 3);
    }

    #[test]
    fn binary_is_not_text() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("b.txt");
        fs::write(&p, b"abc\0def").unwrap();
        assert!(matches!(text_preview(&p), Err(PreviewError::NotText(_))));
        fs::write(&p, [0xff, 0xfe, 0x41]).unwrap();
        assert!(matches!(text_preview(&p), Err(PreviewError::NotText(_))));
    }

    fn fake_qlmanage(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-qlmanage");
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn quick_look_runs_once_then_serves_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        let count = dir.path().join("count");
        // Args: -t -s <size> -o <dir> <file>; writes "<file name>.png" into <dir>.
        let script = fake_qlmanage(
            dir.path(),
            &format!(
                "echo x >> '{}'\nprintf 'PNG:%s' \"$3\" > \"$5/$(basename \"$6\").png\"\n",
                count.display()
            ),
        );
        let ql = QuickLook {
            program: script,
            cache_dir: dir.path().join("cache"),
            ..QuickLook::default()
        };
        let img = dir.path().join("pic.jpg");
        fs::write(&img, "jpg").unwrap();
        assert_eq!(ql.thumbnail(&img, 96).unwrap(), b"PNG:96");
        assert_eq!(ql.thumbnail(&img, 96).unwrap(), b"PNG:96");
        assert_eq!(fs::read_to_string(&count).unwrap().lines().count(), 1, "second call cached");
        assert_eq!(ql.thumbnail(&img, 600).unwrap(), b"PNG:600");
        assert_eq!(fs::read_to_string(&count).unwrap().lines().count(), 2, "other size, new run");
        // Only cached thumbnails remain; run dirs are gone.
        let left: Vec<String> = fs::read_dir(dir.path().join("cache"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
        assert!(left.iter().all(|n| n.ends_with(".png")), "{left:?}");
    }

    #[test]
    fn quick_look_without_output_or_with_bad_exit_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.dmg");
        fs::write(&file, "x").unwrap();

        let silent = QuickLook {
            program: fake_qlmanage(dir.path(), "echo 'No thumbnail created'\n"),
            cache_dir: dir.path().join("cache"),
            ..QuickLook::default()
        };
        let err = silent.thumbnail(&file, 96).unwrap_err();
        assert!(matches!(err, PreviewError::NoThumbnail { .. }), "{err}");
        assert!(err.to_string().contains("No thumbnail created"), "{err}");

        let failing = QuickLook {
            program: fake_qlmanage(dir.path(), "echo boom >&2\nexit 4\n"),
            cache_dir: dir.path().join("cache2"),
            ..QuickLook::default()
        };
        let err = failing.thumbnail(&file, 96).unwrap_err();
        assert!(matches!(err, PreviewError::Failed { .. }), "{err}");

        let missing = QuickLook {
            program: PathBuf::from("/nonexistent/qlmanage"),
            cache_dir: dir.path().join("cache3"),
            ..QuickLook::default()
        };
        assert!(matches!(
            missing.thumbnail(&file, 96),
            Err(PreviewError::NotFound(_))
        ));
        // Failed runs leave no run dir or output file behind.
        for cache in ["cache", "cache2", "cache3"] {
            let left: Vec<_> = fs::read_dir(dir.path().join(cache)).unwrap().collect();
            assert!(left.is_empty(), "{cache}: {left:?}");
        }
    }

    #[test]
    fn a_hung_quick_look_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let pid_file = dir.path().join("pid");
        // `exec` so the pid written is the process that sleeps.
        let script = fake_qlmanage(
            dir.path(),
            &format!(
                "[ -n \"$WARM_UP\" ] && exit 0\necho $$ > '{}'\nexec sleep 30\n",
                pid_file.display()
            ),
        );
        // The first run of a freshly written executable took ~314 ms here
        // (macOS checks it), longer than the short timeout below; get that
        // out of the way first.
        assert!(Command::new(&script).env("WARM_UP", "1").status().unwrap().success());
        let ql = QuickLook {
            program: script,
            cache_dir: dir.path().join("cache"),
            timeout: Duration::from_millis(300),
            ..QuickLook::default()
        };
        let file = dir.path().join("hang.mov");
        fs::write(&file, "x").unwrap();

        let started = Instant::now();
        let err = ql.thumbnail(&file, 96).unwrap_err();
        let took = started.elapsed();
        assert!(matches!(err, PreviewError::Timeout { .. }), "{err}");
        assert!(err.to_string().contains("took longer than 300 ms"), "{err}");
        assert!(took >= Duration::from_millis(300), "{took:?}");
        assert!(took < Duration::from_secs(5), "returned only after {took:?}");

        let pid = fs::read_to_string(&pid_file).unwrap();
        let alive = Command::new("kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "child {} still running", pid.trim());
        let left: Vec<_> = fs::read_dir(dir.path().join("cache")).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn a_timed_out_run_that_wrote_output_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        // Writes its thumbnail, then hangs past the timeout.
        let script = fake_qlmanage(
            dir.path(),
            "[ -n \"$WARM_UP\" ] && exit 0\nprintf PNG > \"$5/$(basename \"$6\").png\"\nexec sleep 30\n",
        );
        // Same warm-up as a_hung_quick_look_is_killed_at_the_timeout.
        assert!(Command::new(&script).env("WARM_UP", "1").status().unwrap().success());
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).unwrap();
        // Files that are not this run's: they must survive the cleanup.
        let other_thumb = cache.join("0000000000000000-96.png");
        fs::write(&other_thumb, "other").unwrap();
        let outside = dir.path().join("outside.png");
        fs::write(&outside, "outside").unwrap();
        let ql = QuickLook {
            program: script,
            cache_dir: cache.clone(),
            timeout: Duration::from_millis(300),
            ..QuickLook::default()
        };
        let file = dir.path().join("hang.mov");
        fs::write(&file, "x").unwrap();

        let err = ql.thumbnail(&file, 96).unwrap_err();
        assert!(matches!(err, PreviewError::Timeout { .. }), "{err}");
        let left: Vec<String> = fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["0000000000000000-96.png"], "only the other thumbnail is left");
        assert_eq!(fs::read(&other_thumb).unwrap(), b"other");
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
    }

    #[test]
    fn a_run_without_a_thumbnail_but_with_other_output_leaves_nothing_behind() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("x.dmg");
        fs::write(&file, "x").unwrap();
        let cache = dir.path().join("cache");

        let no_thumb = QuickLook {
            program: fake_qlmanage(dir.path(), "printf junk > \"$5/notes.txt\"\n"),
            cache_dir: cache.clone(),
            ..QuickLook::default()
        };
        let err = no_thumb.thumbnail(&file, 96).unwrap_err();
        assert!(matches!(err, PreviewError::NoThumbnail { .. }), "{err}");

        let failing = QuickLook {
            program: fake_qlmanage(
                dir.path(),
                "printf PNG > \"$5/$(basename \"$6\").png\"\nexit 3\n",
            ),
            cache_dir: cache.clone(),
            ..QuickLook::default()
        };
        let err = failing.thumbnail(&file, 96).unwrap_err();
        assert!(matches!(err, PreviewError::Failed { .. }), "{err}");

        let left: Vec<_> = fs::read_dir(&cache).unwrap().collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[test]
    fn no_more_than_the_slot_count_run_at_once() {
        let dir = tempfile::tempdir().unwrap();
        let track = dir.path().join("running");
        fs::create_dir(&track).unwrap();
        let counts = dir.path().join("counts");
        // Each run marks itself running, records how many are running now
        // (itself included), holds for a while, then unmarks itself.
        let script = fake_qlmanage(
            dir.path(),
            &format!(
                "T='{t}'\n: > \"$T/run.$$\"\nls \"$T\" | wc -l >> '{c}'\nsleep 0.3\nrm \"$T/run.$$\"\n\
                 printf PNG > \"$5/$(basename \"$6\").png\"\n",
                t = track.display(),
                c = counts.display()
            ),
        );
        let ql = QuickLook {
            program: script,
            cache_dir: dir.path().join("cache"),
            slots: RunSlots::new(2),
            ..QuickLook::default()
        };
        let threads: Vec<_> = (0..6)
            .map(|i| {
                let ql = ql.clone();
                let file = dir.path().join(format!("f{i}.jpg"));
                fs::write(&file, "x").unwrap();
                std::thread::spawn(move || ql.thumbnail(&file, 96).map(|_| ()))
            })
            .collect();
        for t in threads {
            t.join().unwrap().unwrap();
        }
        let seen: Vec<usize> = fs::read_to_string(&counts)
            .unwrap()
            .lines()
            .map(|l| l.trim().parse().unwrap())
            .collect();
        assert_eq!(seen.len(), 6, "{seen:?}");
        assert!(seen.iter().all(|&n| n <= 2), "more than 2 at once: {seen:?}");
        // The cap is reached, so runs really do overlap up to it.
        assert_eq!(seen.iter().max(), Some(&2), "{seen:?}");
    }

    /// Sets a directory's mtime `age` into the past (after its contents are in
    /// place: adding an entry would bump it again).
    fn age_dir(dir: &Path, age: Duration) {
        let f = fs::File::open(dir).unwrap();
        f.set_modified(SystemTime::now() - age).unwrap();
    }

    #[test]
    fn trim_removes_stale_run_dirs_and_keeps_fresh_ones() {
        // T-043 / D-009: a stale run dir holding a file and a subdirectory
        // with a file goes; a younger one (maybe a live run) stays.
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let stale = d.join("run-9-1");
        fs::create_dir_all(stale.join("sub")).unwrap();
        fs::write(stale.join("thumb.png"), "x").unwrap();
        fs::write(stale.join("sub/left.txt"), "y").unwrap();
        age_dir(&stale, STALE_RUN_AGE + Duration::from_secs(5));
        let fresh = d.join("run-9-2");
        fs::create_dir(&fresh).unwrap();
        fs::write(fresh.join("thumb.png"), "x").unwrap();
        age_dir(&fresh, STALE_RUN_AGE - Duration::from_secs(5));
        let not_a_run = d.join("keep-me");
        fs::create_dir(&not_a_run).unwrap();
        age_dir(&not_a_run, STALE_RUN_AGE * 10);

        trim_cache(d, u64::MAX).unwrap();
        assert!(!stale.exists(), "stale run dir (with a subdirectory) is removed");
        assert!(fresh.join("thumb.png").exists(), "a younger run dir is kept");
        assert!(not_a_run.exists(), "only run-* dirs are pruned");
    }

    #[test]
    fn pruning_a_stale_run_dir_does_not_follow_links_out_of_it() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        fs::create_dir(&cache).unwrap();
        let outside = dir.path().join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        let outside_dir = dir.path().join("outside-dir");
        fs::create_dir(&outside_dir).unwrap();
        fs::write(outside_dir.join("f"), "keep").unwrap();
        let stale = cache.join("run-9-3");
        fs::create_dir(&stale).unwrap();
        std::os::unix::fs::symlink(&outside, stale.join("link")).unwrap();
        std::os::unix::fs::symlink(&outside_dir, stale.join("dirlink")).unwrap();
        age_dir(&stale, STALE_RUN_AGE * 2);
        // A run-* entry that is itself a link to a folder is not a run dir.
        std::os::unix::fs::symlink(&outside_dir, cache.join("run-9-4")).unwrap();

        trim_cache(&cache, u64::MAX).unwrap();
        assert!(!stale.exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep", "the link was removed, not its target");
        assert_eq!(fs::read_to_string(outside_dir.join("f")).unwrap(), "keep");
        assert!(cache.join("run-9-4").symlink_metadata().is_ok(), "a run-* link is left alone");
    }

    fn png_of(dir: &Path, name: &str, bytes: usize, age_secs: u64) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, vec![b'x'; bytes]).unwrap();
        let f = fs::File::options().write(true).open(&p).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(age_secs)).unwrap();
        p
    }

    #[test]
    fn cache_is_trimmed_oldest_first_to_its_cap() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        let oldest = png_of(d, "a.png", 100, 300);
        let older = png_of(d, "b.png", 100, 200);
        let newer = png_of(d, "c.png", 100, 100);
        let newest = png_of(d, "d.png", 100, 0);
        // Not cached thumbnails: never counted or removed.
        let partial = d.join("run-1-1.png.partial");
        fs::write(&partial, vec![b'x'; 1000]).unwrap();
        fs::create_dir(d.join("run-1-2")).unwrap();

        trim_cache(d, 400).unwrap();
        assert!(oldest.exists() && older.exists() && newer.exists() && newest.exists());

        trim_cache(d, 250).unwrap();
        assert!(!oldest.exists() && !older.exists(), "the two oldest go first");
        assert!(newer.exists() && newest.exists());
        assert!(partial.exists() && d.join("run-1-2").is_dir());

        trim_cache(d, 0).unwrap();
        assert!(!newer.exists() && !newest.exists());
        assert!(partial.exists());
    }

    #[test]
    fn each_new_thumbnail_trims_the_cache() {
        let dir = tempfile::tempdir().unwrap();
        // Every thumbnail is 10 bytes: "PNG:" + 6 characters of padding.
        let script = fake_qlmanage(dir.path(), "printf 'PNG:xxxxxx' > \"$5/$(basename \"$6\").png\"\n");
        let cache = dir.path().join("cache");
        let ql = QuickLook {
            program: script,
            cache_dir: cache.clone(),
            cache_max_bytes: 25,
            ..QuickLook::default()
        };
        let mut files = vec![];
        for i in 0..4 {
            let file = dir.path().join(format!("f{i}.jpg"));
            fs::write(&file, "x").unwrap();
            assert_eq!(ql.thumbnail(&file, 96).unwrap(), b"PNG:xxxxxx");
            files.push(file);
            // Distinct mtimes, so "oldest" is well defined.
            std::thread::sleep(Duration::from_millis(20));
        }
        let total: u64 = fs::read_dir(&cache)
            .unwrap()
            .map(|e| e.unwrap().metadata().unwrap().len())
            .sum();
        assert_eq!(total, 20, "two 10-byte thumbnails fit in 25 bytes");
        let kept: Vec<bool> = files
            .iter()
            .map(|f| ql.cache_path(f, 96).unwrap().exists())
            .collect();
        assert_eq!(kept, [false, false, true, true], "the newest two are kept");
    }
}
