//! The interactive organizing session behind the web UI.
//!
//! A session holds one scanned folder plus the user's edits on top of the
//! rule classification: manual destinations (including 不搬), checkbox
//! state, and AI suggestions. Every file's *effective* destination is:
//!
//! 1. a manual destination, if the user set one (`source: manual`);
//! 2. otherwise the first matching rule (`source: rule`);
//! 3. otherwise an AI suggestion, if any (`source: ai`, grouped separately
//!    and unchecked by default);
//! 4. otherwise none (`source: none`, stays where it is).
//!
//! Folder edits (add / rename / delete) change the config; the caller
//! persists it. Nothing here moves or deletes a file — moving goes through
//! [`crate::apply`].

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::classify::{Classification, ClassifiedFile, FileInfo, classify};
use crate::config::{Category, Config};
use crate::plan::{Action, Plan, PlanError, build_plan, check_category_name};
use crate::scan::{ScanError, scan};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Rule,
    Manual,
    Ai,
    None,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub folder: String,
    pub reason: String,
}

#[derive(Debug, Clone)]
pub struct Session {
    root: PathBuf,
    recursive: bool,
    files: Vec<FileInfo>,
    sizes: HashMap<PathBuf, u64>,
    /// Manual destination per file; `None` means 不搬.
    overrides: HashMap<PathBuf, Option<String>>,
    /// Explicit checkbox state; absent means the default for its source.
    checked: HashMap<PathBuf, bool>,
    suggestions: HashMap<PathBuf, Suggestion>,
}

#[derive(Debug)]
pub enum SessionError {
    Scan(ScanError),
    Metadata { path: PathBuf, source: std::io::Error },
    UnknownFile(PathBuf),
    UnknownFolder(String),
    BadFolderName(String),
    DuplicateFolder(String),
    NoDestination(PathBuf),
    /// 記住 needs a file the user assigned to a folder by hand.
    NotManuallyAssigned(PathBuf),
    EmptyKeyword,
    KeywordNotInName { keyword: String, name: String },
    Plan(PlanError),
}

impl fmt::Display for SessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SessionError::Scan(e) => write!(f, "{e}"),
            SessionError::Metadata { path, source } => {
                write!(f, "cannot read metadata of {}: {source}", path.display())
            }
            SessionError::UnknownFile(p) => {
                write!(f, "{} is not part of the current scan", p.display())
            }
            SessionError::UnknownFolder(n) => write!(f, "no folder named {n:?}"),
            SessionError::BadFolderName(n) => write!(
                f,
                "{n:?} cannot be a folder name ('/' only between folder names; no empty part, '.', '..' or backslash)"
            ),
            SessionError::DuplicateFolder(n) => write!(f, "a folder named {n:?} already exists"),
            SessionError::NoDestination(p) => write!(
                f,
                "{} has no destination; pick a folder before checking it",
                p.display()
            ),
            SessionError::NotManuallyAssigned(p) => write!(
                f,
                "{} has no folder picked by hand; pick one before remembering it",
                p.display()
            ),
            SessionError::EmptyKeyword => write!(f, "the keyword is empty"),
            SessionError::KeywordNotInName { keyword, name } => write!(
                f,
                "the file name {name:?} does not contain the keyword {keyword:?}"
            ),
            SessionError::Plan(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<PlanError> for SessionError {
    fn from(e: PlanError) -> Self {
        SessionError::Plan(e)
    }
}

/// One file as the preview screen shows it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FileView {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    /// Modified date, `YYYY-MM-DD` local time.
    pub modified: String,
    /// Destination folder name; `None` = 不搬.
    pub dest: Option<String>,
    /// Full destination path, when there is a destination.
    pub to: Option<PathBuf>,
    pub source: Source,
    /// Rule that matched, the AI's reason, or why there is no destination.
    pub reason: String,
    pub checked: bool,
    /// `move`, `collision`, `in_place` or `none`.
    pub status: &'static str,
    /// Why a `collision` cannot be moved.
    pub problem: Option<String>,
    /// How the UI previews it: `image`, `pdf`, `video`, `text` or `other`
    /// ([`crate::preview::kind`]).
    pub preview: &'static str,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct GroupView {
    /// `folder`, `ai` or `none`.
    pub kind: &'static str,
    pub folder: Option<String>,
    /// `<root>/<folder>` for folder groups.
    pub target: Option<PathBuf>,
    pub files: Vec<FileView>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanView {
    pub root: PathBuf,
    pub recursive: bool,
    pub total: usize,
    /// Number of checked files that will be moved.
    pub moving: usize,
    /// One group per configured folder (config order, empty ones too), then
    /// `ai` and `none` when they have files.
    pub groups: Vec<GroupView>,
}

struct Effective {
    dest: Option<String>,
    source: Source,
    reason: String,
}

impl Session {
    /// Scans `root` (which must be an existing directory).
    pub fn scan(root: &Path, recursive: bool) -> Result<Session, SessionError> {
        let files = scan(root, recursive).map_err(SessionError::Scan)?;
        let mut sizes = HashMap::with_capacity(files.len());
        for f in &files {
            let meta = f
                .path
                .symlink_metadata()
                .map_err(|e| SessionError::Metadata {
                    path: f.path.clone(),
                    source: e,
                })?;
            sizes.insert(f.path.clone(), meta.len());
        }
        Ok(Session {
            root: root.to_path_buf(),
            recursive,
            files,
            sizes,
            overrides: HashMap::new(),
            checked: HashMap::new(),
            suggestions: HashMap::new(),
        })
    }

    /// Scans the same folder again, dropping all edits (used after an
    /// apply or undo, when paths have changed).
    pub fn rescan(&self) -> Result<Session, SessionError> {
        Session::scan(&self.root, self.recursive)
    }

    fn file(&self, path: &Path) -> Result<&FileInfo, SessionError> {
        self.files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| SessionError::UnknownFile(path.to_path_buf()))
    }

    fn effective(&self, config: &Config, file: &FileInfo) -> Effective {
        if let Some(dest) = self.overrides.get(&file.path) {
            return Effective {
                dest: dest.clone(),
                source: Source::Manual,
                reason: "手動指定".into(),
            };
        }
        if let Classification::Classified { category, rule } = classify(config, file) {
            return Effective {
                dest: Some(category),
                source: Source::Rule,
                reason: rule,
            };
        }
        if let Some(s) = self.suggestions.get(&file.path) {
            return Effective {
                dest: Some(s.folder.clone()),
                source: Source::Ai,
                reason: s.reason.clone(),
            };
        }
        Effective {
            dest: None,
            source: Source::None,
            reason: "沒有規則符合".into(),
        }
    }

    fn is_checked(&self, path: &Path, eff: &Effective) -> bool {
        if eff.dest.is_none() {
            return false;
        }
        self.checked
            .get(path)
            .copied()
            .unwrap_or(matches!(eff.source, Source::Rule | Source::Manual))
    }

    /// Sets a file's destination by hand (`None` = 不搬). The file becomes
    /// `manual` and is checked exactly when it has a destination.
    pub fn assign(
        &mut self,
        config: &Config,
        path: &Path,
        dest: Option<String>,
    ) -> Result<(), SessionError> {
        self.file(path)?;
        if let Some(d) = &dest
            && config.category(d).is_none()
        {
            return Err(SessionError::UnknownFolder(d.clone()));
        }
        self.checked.insert(path.to_path_buf(), dest.is_some());
        self.overrides.insert(path.to_path_buf(), dest);
        Ok(())
    }

    /// Moves a whole group at once (預覽's 全部移到…): every path gets the
    /// same destination, like [`Session::assign`] on each. All-or-nothing:
    /// the folder and every path are checked first, so an error changes
    /// nothing.
    pub fn assign_all(
        &mut self,
        config: &Config,
        paths: &[PathBuf],
        dest: Option<String>,
    ) -> Result<(), SessionError> {
        if let Some(d) = &dest
            && config.category(d).is_none()
        {
            return Err(SessionError::UnknownFolder(d.clone()));
        }
        for p in paths {
            self.file(p)?;
        }
        for p in paths {
            self.checked.insert(p.clone(), dest.is_some());
            self.overrides.insert(p.clone(), dest.clone());
        }
        Ok(())
    }

    /// 記住: turns a hand-picked destination into a keyword rule. `keyword`
    /// (trimmed) must be non-empty and contained, ignoring case, in the file
    /// name; it is added to the folder the file was assigned to (moved back
    /// from `disabled_keywords` if it was switched off there). The caller
    /// saves the config. Nothing is changed on error.
    pub fn remember_keyword(
        &self,
        config: &mut Config,
        path: &Path,
        keyword: &str,
    ) -> Result<(), SessionError> {
        let file = self.file(path)?;
        let Some(Some(folder)) = self.overrides.get(&file.path) else {
            return Err(SessionError::NotManuallyAssigned(path.to_path_buf()));
        };
        let keyword = keyword.trim();
        if keyword.is_empty() {
            return Err(SessionError::EmptyKeyword);
        }
        let name = file
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if !name.to_lowercase().contains(&keyword.to_lowercase()) {
            return Err(SessionError::KeywordNotInName {
                keyword: keyword.to_owned(),
                name,
            });
        }
        let category = config
            .categories
            .iter_mut()
            .find(|c| &c.name == folder)
            .ok_or_else(|| SessionError::UnknownFolder(folder.clone()))?;
        let lower = keyword.to_lowercase();
        category.disabled_keywords.retain(|k| k.to_lowercase() != lower);
        if !category.keywords.iter().any(|k| k.to_lowercase() == lower) {
            category.keywords.push(keyword.to_owned());
        }
        Ok(())
    }

    /// Checks or unchecks files. Checking a file without a destination is
    /// an error (nothing is changed then).
    pub fn set_checked(
        &mut self,
        config: &Config,
        paths: &[PathBuf],
        checked: bool,
    ) -> Result<(), SessionError> {
        for path in paths {
            let file = self.file(path)?;
            if checked && self.effective(config, file).dest.is_none() {
                return Err(SessionError::NoDestination(path.clone()));
            }
        }
        for path in paths {
            self.checked.insert(path.clone(), checked);
        }
        Ok(())
    }

    /// The scanned folder.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `path` is exactly one of the scanned files (previews are
    /// only served for these).
    pub fn has_file(&self, path: &Path) -> bool {
        self.file(path).is_ok()
    }

    /// Files the AI may be asked about: no rule classifies them and the user
    /// has not picked a destination by hand. `ai_exclude` is applied later,
    /// by [`crate::ai::suggest`].
    pub fn ai_candidates(&self, config: &Config) -> Vec<(FileInfo, u64)> {
        self.files
            .iter()
            .filter(|f| {
                !self.overrides.contains_key(&f.path)
                    && classify(config, f) == Classification::Unclassified
            })
            .map(|f| (f.clone(), self.sizes[&f.path]))
            .collect()
    }

    /// Records AI suggestions (folder per file). They only apply to files
    /// no rule classifies, start unchecked, and must name existing folders.
    /// Replacing earlier suggestions drops the checkmark of every AI file
    /// whose folder is new or changed, so a re-ask never moves a file to a
    /// folder the user did not check; rule and manual files keep theirs.
    pub fn set_suggestions(
        &mut self,
        config: &Config,
        suggestions: HashMap<PathBuf, Suggestion>,
    ) -> Result<(), SessionError> {
        for (path, s) in &suggestions {
            self.file(path)?;
            if config.category(&s.folder).is_none() {
                return Err(SessionError::UnknownFolder(s.folder.clone()));
            }
        }
        let changed: Vec<PathBuf> = self
            .ai_candidates(config)
            .into_iter()
            .map(|(f, _)| f.path)
            .filter(|p| {
                self.suggestions.get(p).map(|s| &s.folder)
                    != suggestions.get(p).map(|s| &s.folder)
            })
            .collect();
        for path in changed {
            self.checked.remove(&path);
        }
        self.suggestions = suggestions;
        Ok(())
    }

    /// Classified inputs for `build_plan`; `only_checked` keeps just the
    /// files that would be moved.
    fn classified(&self, config: &Config, only_checked: bool) -> Vec<(usize, ClassifiedFile, bool)> {
        self.files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let eff = self.effective(config, f);
                let checked = self.is_checked(&f.path, &eff);
                if only_checked && !checked {
                    return None;
                }
                let classification = match eff.dest {
                    Some(category) => Classification::Classified {
                        category,
                        rule: eff.reason,
                    },
                    None => Classification::Unclassified,
                };
                Some((
                    i,
                    ClassifiedFile {
                        path: f.path.clone(),
                        classification,
                    },
                    checked,
                ))
            })
            .collect()
    }

    /// The plan `apply` should execute: checked files only.
    pub fn apply_plan(&self, config: &Config) -> Result<Plan, SessionError> {
        let files: Vec<ClassifiedFile> = self
            .classified(config, true)
            .into_iter()
            .map(|(_, f, _)| f)
            .collect();
        Ok(build_plan(&self.root, &files)?)
    }

    /// The preview, grouped by destination.
    pub fn view(&self, config: &Config) -> Result<PlanView, SessionError> {
        // Checked files claim destinations first, so an unchecked file
        // never makes a checked one look like a collision.
        let mut items = self.classified(config, false);
        items.sort_by_key(|(i, _, checked)| (!checked, *i));
        let files: Vec<ClassifiedFile> = items.iter().map(|(_, f, _)| f.clone()).collect();
        let plan = build_plan(&self.root, &files)?;

        let mut views: Vec<(usize, FileView)> = Vec::with_capacity(items.len());
        for ((i, _, checked), entry) in items.iter().zip(plan.entries) {
            let info = &self.files[*i];
            let eff = self.effective(config, info);
            let (status, to, problem) = match entry.action {
                Action::Move { to, .. } => ("move", Some(to), None),
                Action::Collision { to, reason, .. } => ("collision", Some(to), Some(reason)),
                Action::AlreadyInPlace { .. } => ("in_place", None, None),
                Action::Unclassified => ("none", None, None),
            };
            views.push((
                *i,
                FileView {
                    name: info
                        .path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    path: info.path.clone(),
                    size: self.sizes[&info.path],
                    modified: info.modified.format("%Y-%m-%d").to_string(),
                    dest: eff.dest,
                    to,
                    source: eff.source,
                    reason: eff.reason,
                    checked: *checked && status == "move",
                    status,
                    problem,
                    preview: crate::preview::kind(&info.path),
                },
            ));
        }
        views.sort_by_key(|(i, _)| *i);
        let views: Vec<FileView> = views.into_iter().map(|(_, v)| v).collect();

        let mut groups: Vec<GroupView> = config
            .categories
            .iter()
            .map(|c| GroupView {
                kind: "folder",
                folder: Some(c.name.clone()),
                target: Some(self.root.join(&c.name)),
                files: views
                    .iter()
                    .filter(|v| v.source != Source::Ai && v.dest.as_deref() == Some(&c.name))
                    .cloned()
                    .collect(),
            })
            .collect();
        for (kind, pick) in [
            ("ai", (|v: &FileView| v.source == Source::Ai) as fn(&FileView) -> bool),
            ("none", |v: &FileView| v.source != Source::Ai && v.dest.is_none()),
        ] {
            let files: Vec<FileView> = views.iter().filter(|v| pick(v)).cloned().collect();
            if !files.is_empty() {
                groups.push(GroupView {
                    kind,
                    folder: None,
                    target: None,
                    files,
                });
            }
        }

        Ok(PlanView {
            root: self.root.clone(),
            recursive: self.recursive,
            total: views.len(),
            moving: views.iter().filter(|v| v.checked).count(),
            groups,
        })
    }

    fn rename_refs(&mut self, from: &str, to: &str) {
        for dest in self.overrides.values_mut().flatten() {
            if dest == from {
                *dest = to.to_owned();
            }
        }
        for s in self.suggestions.values_mut() {
            if s.folder == from {
                s.folder = to.to_owned();
            }
        }
    }

    /// Every file whose destination (under `config`) is a folder for which
    /// `gone` is true becomes manual 不搬 and unchecked.
    fn drop_folders(&mut self, config: &Config, gone: impl Fn(&str) -> bool) {
        let affected: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|f| {
                self.effective(config, f)
                    .dest
                    .is_some_and(|d| gone(&d))
            })
            .map(|f| f.path.clone())
            .collect();
        for path in affected {
            self.overrides.insert(path.clone(), None);
            self.checked.insert(path, false);
        }
        self.suggestions.retain(|_, s| !gone(&s.folder));
    }

    /// After the config was replaced wholesale (分類設定): manual
    /// destinations and suggestions naming a folder that no longer exists
    /// become 不搬. Rule-classified files simply follow the new rules.
    pub fn config_replaced(&mut self, new: &Config) {
        let kept: HashSet<&str> = new.categories.iter().map(|c| c.name.as_str()).collect();
        for (path, dest) in self.overrides.iter_mut() {
            if dest.as_deref().is_some_and(|d| !kept.contains(d)) {
                *dest = None;
                self.checked.insert(path.clone(), false);
            }
        }
        self.suggestions.retain(|_, s| kept.contains(s.folder.as_str()));
    }
}

fn valid_new_name(config: &Config, name: &str) -> Result<String, SessionError> {
    let name = name.trim();
    check_category_name(name).map_err(|_| SessionError::BadFolderName(name.to_owned()))?;
    if config.category(name).is_some() {
        return Err(SessionError::DuplicateFolder(name.to_owned()));
    }
    Ok(name.to_owned())
}

/// Adds an empty, rule-less folder (files get there only by hand).
pub fn add_folder(config: &mut Config, name: &str) -> Result<String, SessionError> {
    let name = valid_new_name(config, name)?;
    config.categories.push(Category {
        name: name.clone(),
        extensions: vec![],
        keywords: vec![],
        disabled_keywords: vec![],
        date: None,
    });
    Ok(name)
}

/// Renames a folder: its rules keep applying, its target path becomes
/// `<root>/<to>`, and files assigned or suggested to it follow. Folders
/// already on disk are not renamed.
pub fn rename_folder(
    config: &mut Config,
    session: Option<&mut Session>,
    from: &str,
    to: &str,
) -> Result<String, SessionError> {
    if config.category(from).is_none() {
        return Err(SessionError::UnknownFolder(from.to_owned()));
    }
    if to.trim() == from {
        return Ok(from.to_owned());
    }
    let to = valid_new_name(config, to)?;
    for c in &mut config.categories {
        if c.name == from {
            c.name = to.clone();
        }
    }
    if let Some(s) = session {
        s.rename_refs(from, &to);
    }
    Ok(to)
}

/// Removes a folder and its rules. Files headed there become 不搬; no file
/// on disk is touched.
pub fn delete_folder(
    config: &mut Config,
    session: Option<&mut Session>,
    name: &str,
) -> Result<(), SessionError> {
    if config.category(name).is_none() {
        return Err(SessionError::UnknownFolder(name.to_owned()));
    }
    if let Some(s) = session {
        s.drop_folders(config, |d| d == name);
    }
    config.categories.retain(|c| c.name != name);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn config() -> Config {
        Config::parse(
            "[[category]]\nname = \"Images\"\nextensions = [\"jpg\"]\n\
             [[category]]\nname = \"Docs\"\nextensions = [\"pdf\"]\n\
             [[category]]\nname = \"Other\"\n",
        )
        .unwrap()
    }

    fn fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for name in ["a.jpg", "b.pdf", "c.xyz"] {
            fs::write(root.join(name), name).unwrap();
        }
        (dir, root)
    }

    fn file<'a>(view: &'a PlanView, name: &str) -> &'a FileView {
        view.groups
            .iter()
            .flat_map(|g| &g.files)
            .find(|f| f.name == name)
            .unwrap()
    }

    fn group<'a>(view: &'a PlanView, kind: &str, folder: Option<&str>) -> Option<&'a GroupView> {
        view.groups
            .iter()
            .find(|g| g.kind == kind && g.folder.as_deref() == folder)
    }

    #[test]
    fn rule_files_are_checked_and_grouped_by_folder() {
        let (_d, root) = fixture();
        let s = Session::scan(&root, false).unwrap();
        let v = s.view(&config()).unwrap();
        assert_eq!(v.total, 3);
        assert_eq!(v.moving, 2);
        let images = group(&v, "folder", Some("Images")).unwrap();
        assert_eq!(images.target.as_deref(), Some(root.join("Images").as_path()));
        assert_eq!(images.files[0].name, "a.jpg");
        assert!(images.files[0].checked);
        assert_eq!(images.files[0].source, Source::Rule);
        assert!(group(&v, "folder", Some("Other")).unwrap().files.is_empty());
        assert_eq!(group(&v, "none", None).unwrap().files[0].name, "c.xyz");
    }

    #[test]
    fn ai_suggestions_have_their_own_group_and_start_unchecked() {
        let (_d, root) = fixture();
        let cfg = config();
        let mut s = Session::scan(&root, false).unwrap();
        s.set_suggestions(
            &cfg,
            HashMap::from([(
                root.join("c.xyz"),
                Suggestion {
                    folder: "Docs".into(),
                    reason: "looks like a doc".into(),
                },
            )]),
        )
        .unwrap();
        let v = s.view(&cfg).unwrap();
        let ai = group(&v, "ai", None).unwrap();
        assert_eq!(ai.files[0].name, "c.xyz");
        assert!(!ai.files[0].checked);
        assert!(group(&v, "none", None).is_none());
        assert_eq!(s.apply_plan(&cfg).unwrap().moves().count(), 2, "suggestion not applied");

        // Reassigning it by hand makes it manual and checked.
        s.assign(&cfg, &root.join("c.xyz"), Some("Docs".into())).unwrap();
        let f = file(&s.view(&cfg).unwrap(), "c.xyz").clone();
        assert_eq!((f.source, f.checked), (Source::Manual, true));
    }

    #[test]
    fn checking_a_file_without_destination_is_an_error() {
        let (_d, root) = fixture();
        let cfg = config();
        let mut s = Session::scan(&root, false).unwrap();
        let err = s.set_checked(&cfg, &[root.join("c.xyz")], true).unwrap_err();
        assert!(matches!(err, SessionError::NoDestination(_)));
    }

    #[test]
    fn unchecked_file_does_not_block_a_checked_one() {
        let (_d, root) = fixture();
        fs::create_dir(root.join("sub")).unwrap();
        fs::write(root.join("sub/a.jpg"), "other").unwrap();
        let cfg = config();
        let mut s = Session::scan(&root, true).unwrap();
        s.set_checked(&cfg, &[root.join("a.jpg")], false).unwrap();
        let v = s.view(&cfg).unwrap();
        // sub/a.jpg is checked and claims Images/a.jpg; a.jpg (unchecked) collides.
        let images = &group(&v, "folder", Some("Images")).unwrap().files;
        let sub = images.iter().find(|f| f.path == root.join("sub/a.jpg")).unwrap();
        let top = images.iter().find(|f| f.path == root.join("a.jpg")).unwrap();
        assert_eq!((sub.status, sub.checked), ("move", true));
        assert_eq!(top.status, "collision");
    }

    #[test]
    fn rename_and_delete_folder() {
        let (_d, root) = fixture();
        let mut cfg = config();
        let mut s = Session::scan(&root, false).unwrap();
        s.assign(&cfg, &root.join("c.xyz"), Some("Other".into())).unwrap();

        rename_folder(&mut cfg, Some(&mut s), "Other", "Misc").unwrap();
        let v = s.view(&cfg).unwrap();
        assert_eq!(file(&v, "c.xyz").to.as_deref(), Some(root.join("Misc/c.xyz").as_path()));
        assert!(matches!(
            rename_folder(&mut cfg, Some(&mut s), "Misc", "Images"),
            Err(SessionError::DuplicateFolder(_))
        ));
        assert!(matches!(
            rename_folder(&mut cfg, Some(&mut s), "Misc", "a/../b"),
            Err(SessionError::BadFolderName(_))
        ));
        assert_eq!(
            rename_folder(&mut cfg, Some(&mut s), "Misc", "工作/專案A").unwrap(),
            "工作/專案A"
        );

        delete_folder(&mut cfg, Some(&mut s), "Images").unwrap();
        let a = file(&s.view(&cfg).unwrap(), "a.jpg").clone();
        assert_eq!((a.dest, a.checked, a.source), (None, false, Source::Manual));
        assert!(root.join("a.jpg").exists());
    }

    #[test]
    fn config_replaced_turns_orphaned_manual_choices_into_none() {
        let (_d, root) = fixture();
        let old = config();
        let mut s = Session::scan(&root, false).unwrap();
        s.assign(&old, &root.join("c.xyz"), Some("Other".into())).unwrap();
        let mut new = old.clone();
        new.categories.retain(|c| c.name != "Other");
        s.config_replaced(&new);
        let c = file(&s.view(&new).unwrap(), "c.xyz").clone();
        assert_eq!((c.dest, c.checked), (None, false));
    }
}
