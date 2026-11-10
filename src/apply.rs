//! Apply a move plan and undo the most recent apply.
//!
//! Every apply appends a batch to a JSON move log. The log is saved after
//! each individual move, so an apply interrupted half-way can still be
//! undone. `undo` reverses the newest batch and removes it from the log.
//! Neither operation ever overwrites an existing file.

use std::fmt;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::plan::{Action, Plan};

const LOG_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppliedBatch {
    /// Local time the apply started, RFC 3339.
    pub applied_at: String,
    pub root: PathBuf,
    /// Moves that actually happened, in order.
    pub moves: Vec<Move>,
    /// Category folders this apply created (removed again by undo if empty).
    pub created_dirs: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MoveLog {
    pub version: u32,
    /// Oldest first; undo takes the last one.
    pub applies: Vec<AppliedBatch>,
}

/// A file that was not moved, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    pub from: PathBuf,
    pub to: PathBuf,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ApplyReport {
    pub moved: Vec<Move>,
    pub collisions: Vec<Skipped>,
    /// `None` when there was nothing to move (no batch was logged).
    pub log: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UndoReport {
    /// Moves reversed: `from` is where the file was, `to` its original path.
    pub restored: Vec<Move>,
    /// Moves that could not be reversed; they stay in the log for a retry.
    pub failed: Vec<Skipped>,
    pub removed_dirs: Vec<PathBuf>,
    /// Folders the apply created that are not empty, so were kept.
    pub kept_dirs: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum MoveError {
    LogRead { path: PathBuf, source: std::io::Error },
    LogParse { path: PathBuf, source: serde_json::Error },
    LogVersion { path: PathBuf, version: u32 },
    LogWrite { path: PathBuf, source: std::io::Error },
    LogInPlan(PathBuf),
    NoLog(PathBuf),
    NothingToUndo(PathBuf),
    Io {
        op: &'static str,
        path: PathBuf,
        source: std::io::Error,
        /// How many moves of this apply had already happened (and are logged).
        moved_before: usize,
    },
}

impl fmt::Display for MoveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MoveError::LogRead { path, source } => {
                write!(f, "cannot read move log {}: {source}", path.display())
            }
            MoveError::LogParse { path, source } => {
                write!(f, "move log {} is not valid: {source}", path.display())
            }
            MoveError::LogVersion { path, version } => write!(
                f,
                "move log {} has unsupported version {version} (expected {LOG_VERSION})",
                path.display()
            ),
            MoveError::LogWrite { path, source } => {
                write!(f, "cannot write move log {}: {source}", path.display())
            }
            MoveError::LogInPlan(path) => write!(
                f,
                "the move log {} is itself one of the files the plan would move; put the log elsewhere",
                path.display()
            ),
            MoveError::NoLog(path) => write!(f, "no move log at {}", path.display()),
            MoveError::NothingToUndo(path) => {
                write!(f, "nothing to undo: move log {} has no applies", path.display())
            }
            MoveError::Io {
                op,
                path,
                source,
                moved_before,
            } => write!(
                f,
                "{op} failed for {}: {source} ({moved_before} file(s) were already moved and are logged; run undo to reverse them)",
                path.display()
            ),
        }
    }
}

impl std::error::Error for MoveError {}

impl MoveLog {
    /// Loads the log; a missing file is an empty log.
    pub fn load(path: &Path) -> Result<MoveLog, MoveError> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Ok(MoveLog {
                    version: LOG_VERSION,
                    applies: vec![],
                });
            }
            Err(e) => {
                return Err(MoveError::LogRead {
                    path: path.to_path_buf(),
                    source: e,
                });
            }
        };
        let log: MoveLog = serde_json::from_str(&text).map_err(|e| MoveError::LogParse {
            path: path.to_path_buf(),
            source: e,
        })?;
        if log.version != LOG_VERSION {
            return Err(MoveError::LogVersion {
                path: path.to_path_buf(),
                version: log.version,
            });
        }
        Ok(log)
    }

    /// Writes the whole log via a temp file + rename, so a crash never
    /// leaves a half-written log.
    pub fn save(&self, path: &Path) -> Result<(), MoveError> {
        let write_err = |e| MoveError::LogWrite {
            path: path.to_path_buf(),
            source: e,
        };
        let json = serde_json::to_string_pretty(self).expect("move log serializes");
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        fs::write(&tmp, json).map_err(write_err)?;
        fs::rename(&tmp, path).map_err(write_err)
    }
}

/// Why `dir` must not receive files, if it must not: a folder from just
/// below `root` down to `dir` that already exists as a symbolic link (moving
/// through it could put files outside the scanned folder, D-010), or `dir`
/// not being under `root` at all. Folders above `root` are not checked: on
/// macOS `/tmp` itself is a link to `/private/tmp`.
fn unsafe_destination(root: &Path, dir: &Path) -> Result<Option<String>, std::io::Error> {
    let Ok(rel) = dir.strip_prefix(root) else {
        return Ok(Some(format!(
            "destination folder {} is not under the scanned folder; not moved",
            dir.display()
        )));
    };
    let mut at = root.to_path_buf();
    for part in rel.components() {
        at.push(part);
        match at.symlink_metadata() {
            Ok(m) if m.file_type().is_symlink() => {
                return Ok(Some(format!(
                    "destination folder {} is a symbolic link; not moved",
                    at.display()
                )));
            }
            Ok(_) => {}
            // Everything below a missing level is missing too; apply creates it.
            Err(e) if e.kind() == ErrorKind::NotFound => break,
            Err(e) => return Err(e),
        }
    }
    Ok(None)
}

fn exists(path: &Path) -> Result<bool, std::io::Error> {
    match path.symlink_metadata() {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

/// Moves the file `from` to `to`, failing with `ErrorKind::AlreadyExists`
/// if `to` exists — atomically, unlike an exists-check followed by
/// `fs::rename`, which silently replaces a file that appears in between.
///
/// On macOS this is `renamex_np(from, to, RENAME_EXCL)`: one exclusive
/// rename. It must not be a hard link there: hard-linking an iCloud Drive
/// file that is not downloaded ("dataless") makes macOS download all of
/// it, while a rename leaves it in the cloud (D-005, T-006). Errors, such
/// as `EXDEV` across volumes or `ENOTSUP` on a filesystem without
/// exclusive rename, are returned as is; there is no fallback.
#[cfg(target_os = "macos")]
fn move_no_overwrite(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::ffi::{CString, c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    /// `RENAME_EXCL` from macOS <stdio.h>.
    const RENAME_EXCL: c_uint = 0x0000_0004;
    unsafe extern "C" {
        fn renamex_np(from: *const c_char, to: *const c_char, flags: c_uint) -> c_int;
    }

    let c_path = |p: &Path| {
        CString::new(p.as_os_str().as_bytes()).map_err(|_| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                format!("path contains a NUL byte: {}", p.display()),
            )
        })
    };
    let (c_from, c_to) = (c_path(from)?, c_path(to)?);
    // SAFETY: both pointers come from CStrings that outlive the call, and
    // renamex_np only reads them.
    if unsafe { renamex_np(c_from.as_ptr(), c_to.as_ptr(), RENAME_EXCL) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Elsewhere: `hard_link` (refuses an existing destination) followed by
/// removing the source. Where hard links are unsupported (another
/// filesystem, or one without hard links) the error is returned as is;
/// there is deliberately no fallback to `rename`.
#[cfg(not(target_os = "macos"))]
fn move_no_overwrite(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::hard_link(from, to)?;
    if let Err(e) = fs::remove_file(from) {
        // Undo the link so the file is not left at both paths.
        return match fs::remove_file(to) {
            Ok(()) => Err(e),
            Err(e2) => Err(std::io::Error::new(
                e.kind(),
                format!(
                    "{e}; the file is now at both {} and {} (removing the new link also failed: {e2})",
                    from.display(),
                    to.display()
                ),
            )),
        };
    }
    Ok(())
}

/// Absolute form of `path` for comparing against plan paths (the file
/// itself may not exist yet).
fn absolute(path: &Path) -> PathBuf {
    let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
    let parent = match parent {
        Some(p) => fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
        None => std::env::current_dir().unwrap_or_default(),
    };
    match path.file_name() {
        Some(name) => parent.join(name),
        None => parent,
    }
}

/// Moves every `Move` entry of `plan`, logging each move to `log_path`.
/// Collisions in the plan, and destinations that appeared since the plan
/// was built, are skipped and reported — never overwritten.
pub fn apply(plan: &Plan, log_path: &Path, applied_at: String) -> Result<ApplyReport, MoveError> {
    let log_abs = absolute(log_path);
    if plan.moves().any(|(from, _)| absolute(from) == log_abs) {
        return Err(MoveError::LogInPlan(log_path.to_path_buf()));
    }

    let mut collisions: Vec<Skipped> = plan
        .entries
        .iter()
        .filter_map(|e| match &e.action {
            Action::Collision { to, reason, .. } => Some(Skipped {
                from: e.from.clone(),
                to: to.clone(),
                reason: reason.clone(),
            }),
            _ => None,
        })
        .collect();

    let planned: Vec<(&Path, &Path)> = plan.moves().collect();
    if planned.is_empty() {
        return Ok(ApplyReport {
            moved: vec![],
            collisions,
            log: None,
        });
    }

    let mut log = MoveLog::load(log_path)?;
    log.applies.push(AppliedBatch {
        applied_at,
        root: plan.root.clone(),
        moves: vec![],
        created_dirs: vec![],
    });
    log.save(log_path)?;

    for (from, to) in planned {
        let moved_before = log.applies.last().unwrap().moves.len();
        let io_err = |op, path: &Path, source| MoveError::Io {
            op,
            path: path.to_path_buf(),
            source,
            moved_before,
        };
        let dir = to.parent().expect("planned destination has a parent");
        if let Some(reason) =
            unsafe_destination(&plan.root, dir).map_err(|e| io_err("checking folder", dir, e))?
        {
            collisions.push(Skipped {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                reason,
            });
            continue;
        }
        // Every missing level, outermost first; each is logged right after
        // it is created so undo removes exactly these (innermost first).
        let mut missing = Vec::new();
        for anc in dir.ancestors() {
            if exists(anc).map_err(|e| io_err("checking folder", anc, e))? {
                break;
            }
            missing.push(anc);
        }
        for level in missing.into_iter().rev() {
            fs::create_dir(level).map_err(|e| io_err("creating folder", level, e))?;
            log.applies
                .last_mut()
                .unwrap()
                .created_dirs
                .push(level.to_path_buf());
            log.save(log_path)?;
        }
        // Re-check right before the move: the plan may be stale.
        if exists(to).map_err(|e| io_err("checking destination", to, e))? {
            collisions.push(Skipped {
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                reason: "destination already exists".into(),
            });
            continue;
        }
        match move_no_overwrite(from, to) {
            Ok(()) => {}
            // Appeared between the re-check and the move.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                collisions.push(Skipped {
                    from: from.to_path_buf(),
                    to: to.to_path_buf(),
                    reason: "destination already exists".into(),
                });
                continue;
            }
            Err(e) => return Err(io_err("moving file", from, e)),
        }
        log.applies.last_mut().unwrap().moves.push(Move {
            from: from.to_path_buf(),
            to: to.to_path_buf(),
        });
        log.save(log_path)?;
    }

    Ok(ApplyReport {
        moved: log.applies.last().unwrap().moves.clone(),
        collisions,
        log: Some(log_path.to_path_buf()),
    })
}

/// Reverses the most recent apply in the log. Moves that cannot be
/// reversed (file gone, original path occupied) are reported and stay in
/// the log; the batch is removed only when every move was reversed.
pub fn undo(log_path: &Path) -> Result<UndoReport, MoveError> {
    if !exists(log_path).map_err(|e| MoveError::LogRead {
        path: log_path.to_path_buf(),
        source: e,
    })? {
        return Err(MoveError::NoLog(log_path.to_path_buf()));
    }
    let mut log = MoveLog::load(log_path)?;
    let Some(mut batch) = log.applies.pop() else {
        return Err(MoveError::NothingToUndo(log_path.to_path_buf()));
    };

    let mut restored = Vec::new();
    let mut failed = Vec::new();
    let mut remaining = Vec::new();
    for m in batch.moves.iter().rev() {
        let problem = match (exists(&m.to), exists(&m.from)) {
            (Err(e), _) | (_, Err(e)) => Some(format!("cannot check paths: {e}")),
            (Ok(false), _) => Some("file is no longer at the moved-to path".to_owned()),
            (Ok(true), Ok(true)) => Some("original path is occupied".to_owned()),
            (Ok(true), Ok(false)) => match move_no_overwrite(&m.to, &m.from) {
                Ok(()) => None,
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    Some("original path is occupied".to_owned())
                }
                Err(e) => Some(format!("moving back failed: {e}")),
            },
        };
        match problem {
            None => restored.push(Move {
                from: m.to.clone(),
                to: m.from.clone(),
            }),
            Some(reason) => {
                failed.push(Skipped {
                    from: m.to.clone(),
                    to: m.from.clone(),
                    reason,
                });
                remaining.push(m.clone());
            }
        }
    }

    let mut removed_dirs = Vec::new();
    let mut kept_dirs = Vec::new();
    if remaining.is_empty() {
        for dir in batch.created_dirs.iter().rev() {
            match fs::remove_dir(dir) {
                Ok(()) => removed_dirs.push(dir.clone()),
                Err(e) if e.kind() == ErrorKind::NotFound => removed_dirs.push(dir.clone()),
                // Not empty (or otherwise not removable): keep it, report it.
                Err(_) => kept_dirs.push(dir.clone()),
            }
        }
    } else {
        remaining.reverse();
        batch.moves = remaining;
        log.applies.push(batch);
    }
    log.save(log_path)?;

    Ok(UndoReport {
        restored,
        failed,
        removed_dirs,
        kept_dirs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classify::{Classification, ClassifiedFile};
    use crate::plan::build_plan;

    fn plan_for(root: &Path, files: &[(&str, &str)]) -> Plan {
        let files: Vec<_> = files
            .iter()
            .map(|(name, cat)| ClassifiedFile {
                path: root.join(name),
                classification: Classification::Classified {
                    category: (*cat).into(),
                    rule: "test".into(),
                },
            })
            .collect();
        build_plan(root, &files).unwrap()
    }

    #[test]
    fn apply_then_undo_round_trips_and_removes_created_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.jpg"), "A").unwrap();
        let log = dir.path().join("log.json");

        let report = apply(&plan_for(&root, &[("a.jpg", "Images")]), &log, "t".into()).unwrap();
        assert_eq!(report.moved.len(), 1);
        assert_eq!(fs::read_to_string(root.join("Images/a.jpg")).unwrap(), "A");
        assert!(!root.join("a.jpg").exists());

        let saved = MoveLog::load(&log).unwrap();
        assert_eq!(saved.applies.len(), 1);
        assert_eq!(saved.applies[0].created_dirs, vec![root.join("Images")]);

        let undone = undo(&log).unwrap();
        assert_eq!(undone.restored.len(), 1);
        assert!(undone.failed.is_empty());
        assert_eq!(fs::read_to_string(root.join("a.jpg")).unwrap(), "A");
        assert!(!root.join("Images").exists());
        assert!(MoveLog::load(&log).unwrap().applies.is_empty());
    }

    #[test]
    fn nested_destination_creates_every_missing_level_and_undo_removes_only_those() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("工作")).unwrap(); // already there: not created, not removed
        fs::write(root.join("a.jpg"), "A").unwrap();
        fs::write(root.join("工作/keep.txt"), "K").unwrap();
        let log = dir.path().join("log.json");

        apply(&plan_for(&root, &[("a.jpg", "工作/專案A/2026")]), &log, "t".into()).unwrap();
        assert_eq!(fs::read_to_string(root.join("工作/專案A/2026/a.jpg")).unwrap(), "A");
        assert_eq!(
            MoveLog::load(&log).unwrap().applies[0].created_dirs,
            vec![root.join("工作/專案A"), root.join("工作/專案A/2026")]
        );

        let undone = undo(&log).unwrap();
        assert!(undone.failed.is_empty() && undone.kept_dirs.is_empty());
        assert_eq!(fs::read_to_string(root.join("a.jpg")).unwrap(), "A");
        assert!(!root.join("工作/專案A").exists());
        assert_eq!(fs::read_to_string(root.join("工作/keep.txt")).unwrap(), "K");
    }

    #[test]
    fn move_no_overwrite_refuses_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.jpg");
        let to = dir.path().join("b.jpg");
        fs::write(&from, "source").unwrap();
        fs::write(&to, "destination").unwrap();

        let err = move_no_overwrite(&from, &to).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists);
        assert_eq!(fs::read_to_string(&to).unwrap(), "destination");
        assert_eq!(fs::read_to_string(&from).unwrap(), "source");
    }

    #[test]
    fn move_no_overwrite_moves_to_a_free_destination() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.jpg");
        let to = dir.path().join("b.jpg");
        fs::write(&from, "source").unwrap();

        move_no_overwrite(&from, &to).unwrap();
        assert!(!from.exists());
        assert_eq!(fs::read_to_string(&to).unwrap(), "source");
    }

    #[test]
    fn failed_move_leaves_the_source_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let from = dir.path().join("a.jpg");
        let to = dir.path().join("missing-folder").join("a.jpg");
        fs::write(&from, "source").unwrap();

        let err = move_no_overwrite(&from, &to).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::NotFound);
        assert_eq!(fs::read_to_string(&from).unwrap(), "source");
        assert!(!to.exists());
    }

    #[test]
    fn apply_refuses_a_category_folder_that_is_a_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("a.jpg"), "A").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("Images")).unwrap();

        let report = apply(&plan_for(&root, &[("a.jpg", "Images")]), &dir.path().join("log.json"), "t".into()).unwrap();
        assert!(report.moved.is_empty());
        assert_eq!(report.collisions.len(), 1);
        assert!(report.collisions[0].reason.contains("symbolic link"), "{}", report.collisions[0].reason);
        assert_eq!(fs::read_to_string(root.join("a.jpg")).unwrap(), "A");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "nothing landed outside the root");
    }

    #[test]
    fn apply_refuses_a_nested_category_under_a_symlinked_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let outside = dir.path().join("outside");
        fs::create_dir(&root).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(root.join("x.pdf"), "X").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("a")).unwrap();

        let report = apply(&plan_for(&root, &[("x.pdf", "a/b")]), &dir.path().join("log.json"), "t".into()).unwrap();
        assert!(report.moved.is_empty());
        assert!(report.collisions[0].reason.contains(&root.join("a").display().to_string()));
        assert_eq!(fs::read_to_string(root.join("x.pdf")).unwrap(), "X");
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 0, "a/b was not created through the link");
    }

    #[test]
    fn apply_still_moves_into_a_normal_nested_folder() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("a")).unwrap(); // a real folder, b missing
        fs::write(root.join("x.pdf"), "X").unwrap();

        let report = apply(&plan_for(&root, &[("x.pdf", "a/b")]), &dir.path().join("log.json"), "t".into()).unwrap();
        assert_eq!(report.moved.len(), 1);
        assert!(report.collisions.is_empty());
        assert_eq!(fs::read_to_string(root.join("a/b/x.pdf")).unwrap(), "X");
    }

    #[test]
    fn apply_skips_destination_that_appeared_after_planning() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("a.jpg"), "new").unwrap();
        let plan = plan_for(&root, &[("a.jpg", "Images")]);
        // Destination shows up between plan and apply.
        fs::create_dir(root.join("Images")).unwrap();
        fs::write(root.join("Images/a.jpg"), "old").unwrap();

        let report = apply(&plan, &dir.path().join("log.json"), "t".into()).unwrap();
        assert!(report.moved.is_empty());
        assert_eq!(report.collisions.len(), 1);
        assert_eq!(fs::read_to_string(root.join("Images/a.jpg")).unwrap(), "old");
        assert_eq!(fs::read_to_string(root.join("a.jpg")).unwrap(), "new");
    }

    #[test]
    fn undo_reverses_only_the_most_recent_apply() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.jpg"), "A").unwrap();
        fs::write(root.join("b.pdf"), "B").unwrap();
        let log = dir.path().join("log.json");

        apply(&plan_for(&root, &[("a.jpg", "Images")]), &log, "1".into()).unwrap();
        apply(&plan_for(&root, &[("b.pdf", "Docs")]), &log, "2".into()).unwrap();
        undo(&log).unwrap();

        assert!(root.join("b.pdf").exists());
        assert!(root.join("Images/a.jpg").exists());
        let saved = MoveLog::load(&log).unwrap();
        assert_eq!(saved.applies.len(), 1);
        assert_eq!(saved.applies[0].applied_at, "1");
    }

    #[test]
    fn undo_does_not_overwrite_an_occupied_original_path_and_keeps_it_logged() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.jpg"), "A").unwrap();
        let log = dir.path().join("log.json");
        apply(&plan_for(&root, &[("a.jpg", "Images")]), &log, "t".into()).unwrap();
        fs::write(root.join("a.jpg"), "someone else").unwrap();

        let report = undo(&log).unwrap();
        assert!(report.restored.is_empty());
        assert_eq!(report.failed[0].reason, "original path is occupied");
        assert_eq!(fs::read_to_string(root.join("a.jpg")).unwrap(), "someone else");
        assert_eq!(fs::read_to_string(root.join("Images/a.jpg")).unwrap(), "A");
        assert_eq!(MoveLog::load(&log).unwrap().applies[0].moves.len(), 1);
    }

    #[test]
    fn undo_keeps_created_folder_that_is_not_empty() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.jpg"), "A").unwrap();
        let log = dir.path().join("log.json");
        apply(&plan_for(&root, &[("a.jpg", "Images")]), &log, "t".into()).unwrap();
        fs::write(root.join("Images/added-later.jpg"), "x").unwrap();

        let report = undo(&log).unwrap();
        assert_eq!(report.kept_dirs, vec![root.join("Images")]);
        assert!(root.join("Images/added-later.jpg").exists());
    }

    #[test]
    fn undo_without_log_or_applies_fails_loudly() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log.json");
        assert!(matches!(undo(&log), Err(MoveError::NoLog(_))));
        MoveLog {
            version: LOG_VERSION,
            applies: vec![],
        }
        .save(&log)
        .unwrap();
        assert!(matches!(undo(&log), Err(MoveError::NothingToUndo(_))));
    }

    #[test]
    fn corrupt_log_is_an_error_not_an_empty_log() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("a.jpg"), "A").unwrap();
        let log = dir.path().join("log.json");
        fs::write(&log, "not json").unwrap();
        let err = apply(&plan_for(&root, &[("a.jpg", "Images")]), &log, "t".into()).unwrap_err();
        assert!(matches!(err, MoveError::LogParse { .. }));
        assert!(root.join("a.jpg").exists(), "nothing moved");
        assert_eq!(fs::read_to_string(&log).unwrap(), "not json", "log untouched");
    }

    #[test]
    fn log_inside_the_plan_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("log.json"), "").unwrap();
        let plan = plan_for(&root, &[("log.json", "Data")]);
        let err = apply(&plan, &root.join("log.json"), "t".into()).unwrap_err();
        assert!(matches!(err, MoveError::LogInPlan(_)));
    }
}
