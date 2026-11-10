//! AI suggestions for files no rule classifies, via the user's local,
//! logged-in Claude Code CLI (`claude -p`, subscription quota, no API key).
//!
//! - Only files the caller passes in are considered; the caller passes the
//!   ones left unclassified by the rules ([`crate::session::Session::ai_candidates`]).
//! - Files matching `ai_exclude` ([`is_excluded`]) never reach the prompt.
//! - Only metadata is sent: path relative to the scanned folder, size and
//!   modified date. File contents are never read or sent.
//! - The CLI runs with every tool disabled, so the model can only answer; it
//!   cannot read or touch files. `--tools ""` removes the built-in tools, and
//!   `--strict-mcp-config` (with no `--mcp-config`) loads no MCP servers:
//!   without it the user's MCP tools (claude.ai connectors, plugins) stay
//!   callable, checked against the real CLI 2.1.319 in T-013.
//! - Files are sent in batches of [`BATCH_SIZE`] per call. The result is
//!   all-or-nothing: any failed call or any invalid answer fails the whole
//!   step with an error, and no suggestion is returned.
//! - Suggestions are only suggestions: the session shows them unchecked
//!   and nothing moves until the user checks them and confirms the apply.
//!
//! The backend sits behind [`AiBackend`] so tests use a mock and never run
//! the real CLI.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::classify::FileInfo;
use crate::config::Config;
use crate::session::Suggestion;

/// Files per CLI call.
pub const BATCH_SIZE: usize = 100;

/// Sends one prompt, returns the model's raw text answer.
pub trait AiBackend: Send + Sync {
    fn complete(&self, prompt: &str) -> Result<String, AiError>;
}

#[derive(Debug)]
pub enum AiError {
    /// The CLI binary could not be found.
    NotFound(PathBuf),
    Spawn { program: PathBuf, source: std::io::Error },
    /// The CLI ran but exited non-zero (or was killed).
    Failed { program: PathBuf, status: String, stderr: String },
    /// The answer is not usable; nothing from it was applied.
    InvalidOutput(String),
    /// An `ai_exclude` entry cannot be evaluated.
    BadExclude(String),
}

impl fmt::Display for AiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AiError::NotFound(p) => write!(
                f,
                "AI step failed: {} not found; install Claude Code and log in (`claude`), or classify these files by hand",
                p.display()
            ),
            AiError::Spawn { program, source } => {
                write!(f, "AI step failed: cannot run {}: {source}", program.display())
            }
            AiError::Failed {
                program,
                status,
                stderr,
            } => write!(
                f,
                "AI step failed: {} exited with {status}: {}",
                program.display(),
                stderr.trim()
            ),
            AiError::InvalidOutput(msg) => {
                write!(f, "AI step failed: unusable answer, nothing applied: {msg}")
            }
            AiError::BadExclude(msg) => write!(f, "AI step failed: {msg}"),
        }
    }
}

impl std::error::Error for AiError {}

/// Runs `claude -p` as a subprocess, prompt on stdin, tools disabled.
#[derive(Debug, Clone)]
pub struct ClaudeCli {
    pub program: PathBuf,
    pub model: Option<String>,
}

impl Default for ClaudeCli {
    fn default() -> Self {
        ClaudeCli {
            program: PathBuf::from("claude"),
            model: None,
        }
    }
}

impl ClaudeCli {
    /// Arguments passed to the CLI (the prompt goes to stdin, so it can
    /// never be mistaken for a flag or swallowed by the variadic `--tools`).
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = [
            "-p",
            "--no-session-persistence",
            "--output-format",
            "text",
        ]
        .map(String::from)
        .to_vec();
        if let Some(m) = &self.model {
            args.push("--model".into());
            args.push(m.clone());
        }
        // No MCP servers at all: `--tools ""` alone leaves MCP tools loaded.
        args.push("--strict-mcp-config".into());
        // `--tools ""` disables every built-in tool.
        args.push("--tools".into());
        args.push(String::new());
        args
    }
}

impl AiBackend for ClaudeCli {
    fn complete(&self, prompt: &str) -> Result<String, AiError> {
        let mut child = Command::new(&self.program)
            .args(self.args())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::NotFound => AiError::NotFound(self.program.clone()),
                _ => AiError::Spawn {
                    program: self.program.clone(),
                    source: e,
                },
            })?;
        // Write stdin on another thread so a child that writes a lot before
        // reading everything cannot deadlock us.
        let mut stdin = child.stdin.take().expect("stdin is piped");
        let input = prompt.to_owned();
        let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
        let output = child.wait_with_output().map_err(|e| AiError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;
        let write_result = writer.join().expect("stdin writer thread panicked");
        if !output.status.success() {
            return Err(AiError::Failed {
                program: self.program.clone(),
                status: output.status.to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            });
        }
        // Exited 0 but did not take the whole prompt: its answer is not about
        // our files.
        write_result.map_err(|e| AiError::Spawn {
            program: self.program.clone(),
            source: e,
        })?;
        String::from_utf8(output.stdout)
            .map_err(|_| AiError::InvalidOutput("answer is not UTF-8".into()))
    }
}

/// Whether `file` (under the scanned folder `root`) matches an
/// `ai_exclude` entry:
/// - `*.ext` (or `*.tar.gz`): the file name ends with `.ext`, case-insensitive;
/// - `~/dir` or `/abs/dir`: the file is inside that folder;
/// - a relative `dir` or `a/b`: those folder names appear, in order, in the
///   file's folder path below `root` (so `tax` excludes `root/tax/x` and
///   `root/2024/tax/y`).
pub fn is_excluded(config: &Config, root: &Path, file: &Path) -> Result<bool, AiError> {
    let name = file
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let rel_dirs: Vec<Component> = file
        .strip_prefix(root)
        .ok()
        .and_then(Path::parent)
        .map(|p| p.components().collect())
        .unwrap_or_default();
    for entry in &config.ai_exclude {
        let entry = entry.trim();
        if let Some(suffix) = entry.strip_prefix('*') {
            if !suffix.starts_with('.') || suffix.len() < 2 {
                return Err(AiError::BadExclude(format!(
                    "ai_exclude entry {entry:?}: patterns must look like *.ext"
                )));
            }
            if name.ends_with(&suffix.to_lowercase()) {
                return Ok(true);
            }
            continue;
        }
        let dir = if entry == "~" || entry.starts_with("~/") {
            let home = std::env::var_os("HOME").ok_or_else(|| {
                AiError::BadExclude(format!("ai_exclude entry {entry:?}: HOME is not set"))
            })?;
            PathBuf::from(home).join(entry.trim_start_matches('~').trim_start_matches('/'))
        } else {
            PathBuf::from(entry)
        };
        if dir.is_absolute() {
            if file.starts_with(&dir) {
                return Ok(true);
            }
            continue;
        }
        let want: Vec<Component> = dir
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        if want.is_empty() {
            return Err(AiError::BadExclude(format!(
                "ai_exclude entry {entry:?} names no folder"
            )));
        }
        if rel_dirs.windows(want.len()).any(|w| w == want.as_slice()) {
            return Ok(true);
        }
    }
    Ok(false)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Answer {
    suggestions: Vec<AnswerItem>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AnswerItem {
    id: usize,
    folder: String,
    reason: String,
}

fn build_prompt(config: &Config, root: &Path, batch: &[(usize, &FileInfo, u64)]) -> String {
    let folders: Vec<&str> = config.categories.iter().map(|c| c.name.as_str()).collect();
    let mut prompt = String::from(
        "You help sort files into folders. You only see file metadata (name, size, modified date), never contents.\n\
         For each file below, pick the single best folder from the folder list, or leave the file out if no folder fits.\n\
         Reply with only a JSON object and nothing else, exactly in this shape:\n\
         {\"suggestions\":[{\"id\":<file id>,\"folder\":\"<a folder name from the list, verbatim>\",\"reason\":\"<one short reason in Traditional Chinese>\"}]}\n\n",
    );
    prompt.push_str("Folders (JSON array): ");
    prompt.push_str(&serde_json::to_string(&folders).expect("strings serialize"));
    prompt.push_str("\n\nFiles (one JSON object per line):\n");
    for (id, file, size) in batch {
        let rel = file.path.strip_prefix(root).unwrap_or(&file.path);
        let line = serde_json::json!({
            "id": id,
            "path": rel.to_string_lossy(),
            "size_bytes": size,
            "modified": file.modified.format("%Y-%m-%d").to_string(),
        });
        prompt.push_str(&line.to_string());
        prompt.push('\n');
    }
    prompt
}

/// Strips one optional Markdown code fence around the JSON.
fn unfence(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let Some(body) = rest.strip_suffix("```") else {
        return t;
    };
    // Drop an info string such as `json` on the opening line.
    match body.split_once('\n') {
        Some((info, inner)) if !info.trim_start().starts_with('{') => inner.trim(),
        _ => body.trim(),
    }
}

fn parse_answer(
    text: &str,
    config: &Config,
    batch: &[(usize, &FileInfo, u64)],
) -> Result<Vec<(PathBuf, Suggestion)>, AiError> {
    let answer: Answer = serde_json::from_str(unfence(text)).map_err(|e| {
        let head: String = text.trim().chars().take(200).collect();
        AiError::InvalidOutput(format!("not the expected JSON ({e}); answer began: {head:?}"))
    })?;
    let by_id: HashMap<usize, &FileInfo> = batch.iter().map(|(id, f, _)| (*id, *f)).collect();
    let mut seen = HashSet::new();
    let mut out = Vec::with_capacity(answer.suggestions.len());
    for item in answer.suggestions {
        let file = by_id.get(&item.id).ok_or_else(|| {
            AiError::InvalidOutput(format!("suggestion for unknown file id {}", item.id))
        })?;
        if !seen.insert(item.id) {
            return Err(AiError::InvalidOutput(format!(
                "more than one suggestion for file id {}",
                item.id
            )));
        }
        if config.category(&item.folder).is_none() {
            return Err(AiError::InvalidOutput(format!(
                "file id {} was put in {:?}, which is not one of the folders",
                item.id, item.folder
            )));
        }
        let reason = item.reason.trim();
        if reason.is_empty() {
            return Err(AiError::InvalidOutput(format!(
                "suggestion for file id {} has no reason",
                item.id
            )));
        }
        out.push((
            file.path.clone(),
            Suggestion {
                folder: item.folder,
                reason: reason.to_owned(),
            },
        ));
    }
    Ok(out)
}

/// Asks the backend for a folder for each of `files` (with their sizes),
/// skipping excluded ones. Returns suggestions only if every batch call
/// succeeded and every answer was valid.
pub fn suggest(
    backend: &dyn AiBackend,
    config: &Config,
    root: &Path,
    files: &[(&FileInfo, u64)],
) -> Result<HashMap<PathBuf, Suggestion>, AiError> {
    let mut sendable: Vec<(usize, &FileInfo, u64)> = Vec::new();
    for (file, size) in files {
        if !is_excluded(config, root, &file.path)? {
            sendable.push((sendable.len() + 1, *file, *size));
        }
    }
    let mut all = HashMap::new();
    if sendable.is_empty() || config.categories.is_empty() {
        return Ok(all);
    }
    for batch in sendable.chunks(BATCH_SIZE) {
        let answer = backend.complete(&build_prompt(config, root, batch))?;
        all.extend(parse_answer(&answer, config, batch)?);
    }
    Ok(all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    use chrono::NaiveDate;

    /// Records prompts; answers with the canned replies in order.
    struct Mock {
        prompts: Mutex<Vec<String>>,
        replies: Mutex<Vec<Result<String, AiError>>>,
    }

    impl Mock {
        fn new(replies: Vec<Result<String, AiError>>) -> Mock {
            Mock {
                prompts: Mutex::new(vec![]),
                replies: Mutex::new(replies),
            }
        }
        fn prompts(&self) -> Vec<String> {
            self.prompts.lock().unwrap().clone()
        }
    }

    impl AiBackend for Mock {
        fn complete(&self, prompt: &str) -> Result<String, AiError> {
            self.prompts.lock().unwrap().push(prompt.to_owned());
            self.replies.lock().unwrap().remove(0)
        }
    }

    fn config(exclude: &[&str]) -> Config {
        let mut c = Config::parse(
            "[[category]]\nname = \"文件\"\nextensions = [\"pdf\"]\n[[category]]\nname = \"Other\"\n",
        )
        .unwrap();
        c.ai_exclude = exclude.iter().map(|s| s.to_string()).collect();
        c
    }

    fn info(root: &Path, rel: &str) -> FileInfo {
        FileInfo {
            path: root.join(rel),
            modified: NaiveDate::from_ymd_opt(2026, 10, 1).unwrap(),
        }
    }

    fn sized(files: &[FileInfo]) -> Vec<(&FileInfo, u64)> {
        files.iter().map(|f| (f, 10)).collect()
    }

    const ROOT: &str = "/scan/root";

    #[test]
    fn excluded_folders_and_extensions_never_reach_the_prompt() {
        let root = Path::new(ROOT);
        let home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let files = vec![
            info(root, "notes.xyz"),
            info(root, "tax/return.xyz"),
            info(root, "2024/tax/deep.xyz"),
            info(root, "private/a/secret.xyz"),
            info(root, "deck.KEY"),
            info(root, "backup.tar.gz"),
            info(root, "taxi/ok.xyz"),
        ];
        let mut files = files;
        // An absolute entry and a ~/ entry, for a root under HOME.
        let home_root = home.join("folder-tidy-test-root");
        files.push(info(&home_root, "hidden/h.xyz"));
        let cfg = config(&["tax", "private/a", "*.key", "*.tar.gz", "~/folder-tidy-test-root/hidden", "/abs/never"]);

        let mock = Mock::new(vec![Ok(r#"{"suggestions":[]}"#.into())]);
        suggest(&mock, &cfg, root, &sized(&files)).unwrap();
        let prompts = mock.prompts();
        assert_eq!(prompts.len(), 1);
        let p = &prompts[0];
        for hidden in ["return.xyz", "deep.xyz", "secret.xyz", "deck.KEY", "backup.tar.gz", "h.xyz"] {
            assert!(!p.contains(hidden), "{hidden} leaked into the prompt:\n{p}");
        }
        assert!(p.contains("notes.xyz"), "{p}");
        assert!(p.contains("taxi/ok.xyz"), "'tax' must not match 'taxi': {p}");
        assert!(p.contains(r#"["文件","Other"]"#), "{p}");
    }

    #[test]
    fn nothing_sendable_means_no_call() {
        let root = Path::new(ROOT);
        let files = vec![info(root, "a.key")];
        let mock = Mock::new(vec![]);
        let out = suggest(&mock, &config(&["*.key"]), root, &sized(&files)).unwrap();
        assert!(out.is_empty());
        assert!(mock.prompts().is_empty());
    }

    #[test]
    fn bad_exclude_pattern_is_an_error() {
        let root = Path::new(ROOT);
        let files = vec![info(root, "a.xyz")];
        let mock = Mock::new(vec![]);
        let err = suggest(&mock, &config(&["*key"]), root, &sized(&files)).unwrap_err();
        assert!(matches!(err, AiError::BadExclude(_)), "{err}");
        assert!(mock.prompts().is_empty());
    }

    #[test]
    fn valid_answer_maps_ids_to_paths_and_fenced_json_is_accepted() {
        let root = Path::new(ROOT);
        let files = vec![info(root, "a.xyz"), info(root, "b.xyz")];
        let mock = Mock::new(vec![Ok(
            "```json\n{\"suggestions\":[{\"id\":2,\"folder\":\"文件\",\"reason\":\"像是文件\"}]}\n```\n".into(),
        )]);
        let out = suggest(&mock, &config(&[]), root, &sized(&files)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[&root.join("b.xyz")],
            Suggestion {
                folder: "文件".into(),
                reason: "像是文件".into()
            }
        );
    }

    #[test]
    fn invalid_answers_are_rejected_whole() {
        let root = Path::new(ROOT);
        let files = vec![info(root, "a.xyz"), info(root, "b.xyz")];
        let good = r#"{"id":1,"folder":"文件","reason":"ok"}"#;
        for bad in [
            "Sure! Here you go: a.xyz -> 文件".to_string(),
            String::new(),
            format!(r#"{{"suggestions":[{good},{{"id":2,"folder":"Nope","reason":"x"}}]}}"#),
            format!(r#"{{"suggestions":[{good},{{"id":9,"folder":"文件","reason":"x"}}]}}"#),
            format!(r#"{{"suggestions":[{good},{good}]}}"#),
            format!(r#"{{"suggestions":[{good},{{"id":2,"folder":"文件","reason":" "}}]}}"#),
            format!(r#"{{"suggestions":[{good},{{"id":2,"folder":"文件"}}]}}"#),
            format!(r#"{{"suggestions":[{good}],"extra":1}}"#),
        ] {
            let mock = Mock::new(vec![Ok(bad.clone())]);
            let err = suggest(&mock, &config(&[]), root, &sized(&files)).unwrap_err();
            assert!(matches!(err, AiError::InvalidOutput(_)), "{bad:?} gave {err}");
        }
    }

    #[test]
    fn many_files_are_batched_and_one_bad_batch_fails_everything() {
        let root = Path::new(ROOT);
        let files: Vec<FileInfo> = (0..BATCH_SIZE + 5)
            .map(|i| info(root, &format!("f{i}.xyz")))
            .collect();
        let first = format!(r#"{{"suggestions":[{{"id":1,"folder":"文件","reason":"ok"}}]}}"#);
        let mock = Mock::new(vec![Ok(first.clone()), Ok("garbage".into())]);
        let err = suggest(&mock, &config(&[]), root, &sized(&files)).unwrap_err();
        assert!(matches!(err, AiError::InvalidOutput(_)));
        assert_eq!(mock.prompts().len(), 2, "two batches");
        assert!(mock.prompts()[1].contains(&format!("\"id\":{}", BATCH_SIZE + 1)));

        let mock = Mock::new(vec![
            Ok(first),
            Ok(format!(r#"{{"suggestions":[{{"id":{},"folder":"Other","reason":"ok"}}]}}"#, BATCH_SIZE + 5)),
        ]);
        let out = suggest(&mock, &config(&[]), root, &sized(&files)).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[&root.join(format!("f{}.xyz", BATCH_SIZE + 4))].folder, "Other");
    }

    #[test]
    fn backend_error_is_passed_through() {
        let root = Path::new(ROOT);
        let files = vec![info(root, "a.xyz")];
        let mock = Mock::new(vec![Err(AiError::NotFound("claude".into()))]);
        let err = suggest(&mock, &config(&[]), root, &sized(&files)).unwrap_err();
        assert!(matches!(err, AiError::NotFound(_)));
    }

    // ---- the subprocess backend, against stand-in programs (never the real claude) ----

    #[test]
    fn missing_binary_is_an_explicit_error() {
        let cli = ClaudeCli {
            program: PathBuf::from("/nonexistent/dir/claude"),
            model: None,
        };
        let err = cli.complete("hi").unwrap_err();
        assert!(matches!(err, AiError::NotFound(_)), "{err}");
        assert!(err.to_string().contains("/nonexistent/dir/claude"), "{err}");
    }

    #[test]
    fn non_zero_exit_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let script = fake_cli(dir.path(), "echo 'not logged in' >&2\nexit 3\n");
        let err = ClaudeCli {
            program: script,
            model: None,
        }
        .complete("hi")
        .unwrap_err();
        match &err {
            AiError::Failed { stderr, .. } => assert!(stderr.contains("not logged in")),
            other => panic!("expected Failed, got {other}"),
        }
        assert!(err.to_string().contains("not logged in"), "{err}");
    }

    #[test]
    fn prompt_goes_to_stdin_with_tools_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let args_file = dir.path().join("args");
        let body = format!(
            "for a in \"$@\"; do printf '[%s]' \"$a\"; done > '{}'\ncat\n",
            args_file.display()
        );
        let script = fake_cli(dir.path(), &body);
        let cli = ClaudeCli {
            program: script,
            model: Some("sonnet".into()),
        };
        let prompt = "line 1\n--tools Bash\n".repeat(20_000); // > pipe buffer
        assert_eq!(cli.complete(&prompt).unwrap(), prompt);
        assert_eq!(
            std::fs::read_to_string(&args_file).unwrap(),
            "[-p][--no-session-persistence][--output-format][text][--model][sonnet][--strict-mcp-config][--tools][]"
        );
    }

    fn fake_cli(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-claude");
        std::fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
}
