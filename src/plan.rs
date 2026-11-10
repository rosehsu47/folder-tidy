//! Build a move plan from classified files.
//!
//! Each classified file is planned to go to `<root>/<category>/<file name>`.
//! Unclassified files stay where they are. A destination that already
//! exists (or that another file in the same plan already claims) is a
//! collision: it is reported and never overwritten. Building a plan only
//! reads the disk (to check whether destinations exist); it moves nothing.

use std::collections::HashSet;
use std::fmt;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use serde::Serialize;

use crate::classify::{Classification, ClassifiedFile};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Action {
    /// The file will be moved to `to`.
    Move {
        category: String,
        rule: String,
        to: PathBuf,
    },
    /// The file would go to `to`, but it cannot without overwriting.
    Collision {
        category: String,
        rule: String,
        to: PathBuf,
        reason: String,
    },
    /// The file is already at its destination.
    AlreadyInPlace { category: String },
    /// No rule matched; the file is left alone.
    Unclassified,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanEntry {
    pub from: PathBuf,
    #[serde(flatten)]
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Plan {
    pub root: PathBuf,
    pub entries: Vec<PlanEntry>,
}

impl Plan {
    /// `(from, to)` of every entry that will be moved.
    pub fn moves(&self) -> impl Iterator<Item = (&Path, &Path)> {
        self.entries.iter().filter_map(|e| match &e.action {
            Action::Move { to, .. } => Some((e.from.as_path(), to.as_path())),
            _ => None,
        })
    }
}

#[derive(Debug)]
pub enum PlanError {
    /// A category name with an empty, `.` or `..` segment, a leading `/` or a
    /// backslash could place files outside the scanned folder.
    BadCategoryName(String),
    NoFileName(PathBuf),
}

impl fmt::Display for PlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanError::BadCategoryName(n) => write!(
                f,
                "category name {n:?} cannot be used as a folder name (use '/' only between folder names; no empty part, '.', '..' or backslash)"
            ),
            PlanError::NoFileName(p) => write!(f, "file has no name: {}", p.display()),
        }
    }
}

impl std::error::Error for PlanError {}

/// A category name is one or more folder names separated by `/`
/// (`工作/專案A` nests). Every segment must be a plain folder name, so
/// the folder always stays under the scanned root: no empty segment (which
/// also rejects a leading, trailing or doubled `/`), no `.` or `..`, no
/// backslash.
pub fn check_category_name(name: &str) -> Result<(), PlanError> {
    let ok = !name.contains('\\')
        && name.split('/').all(|seg| {
            let mut components = Path::new(seg).components();
            matches!(components.next(), Some(Component::Normal(_)))
                && components.next().is_none()
        });
    if !ok {
        return Err(PlanError::BadCategoryName(name.to_owned()));
    }
    Ok(())
}

/// Why `to` cannot be used as a destination, or `None` if it is free.
fn occupied(to: &Path) -> Option<String> {
    match to.symlink_metadata() {
        Ok(_) => Some("destination already exists".into()),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => Some(format!("cannot check destination: {e}")),
    }
}

/// Plans where each file goes. `root` is the scanned folder; category
/// folders are created directly under it.
pub fn build_plan(root: &Path, files: &[ClassifiedFile]) -> Result<Plan, PlanError> {
    let mut claimed: HashSet<PathBuf> = HashSet::new();
    let mut entries = Vec::with_capacity(files.len());
    for file in files {
        let action = match &file.classification {
            Classification::Unclassified => Action::Unclassified,
            Classification::Classified { category, rule } => {
                check_category_name(category)?;
                let name = file
                    .path
                    .file_name()
                    .ok_or_else(|| PlanError::NoFileName(file.path.clone()))?;
                let to = root.join(category).join(name);
                if to == file.path {
                    Action::AlreadyInPlace {
                        category: category.clone(),
                    }
                } else if let Some(reason) = occupied(&to) {
                    Action::Collision {
                        category: category.clone(),
                        rule: rule.clone(),
                        to,
                        reason,
                    }
                } else if !claimed.insert(to.clone()) {
                    Action::Collision {
                        category: category.clone(),
                        rule: rule.clone(),
                        to,
                        reason: "another file in this plan has the same destination".into(),
                    }
                } else {
                    Action::Move {
                        category: category.clone(),
                        rule: rule.clone(),
                        to,
                    }
                }
            }
        };
        entries.push(PlanEntry {
            from: file.path.clone(),
            action,
        });
    }
    Ok(Plan {
        root: root.to_path_buf(),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn classified(path: PathBuf, category: &str) -> ClassifiedFile {
        ClassifiedFile {
            path,
            classification: Classification::Classified {
                category: category.into(),
                rule: "test".into(),
            },
        }
    }

    #[test]
    fn plans_category_folders_under_root_and_skips_unclassified() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files = vec![
            classified(root.join("a.jpg"), "Images"),
            classified(root.join("sub/b.jpg"), "Images"),
            ClassifiedFile {
                path: root.join("c.xyz"),
                classification: Classification::Unclassified,
            },
        ];
        let plan = build_plan(root, &files).unwrap();
        let moves: Vec<_> = plan.moves().collect();
        assert_eq!(
            moves,
            vec![
                (root.join("a.jpg").as_path(), root.join("Images/a.jpg").as_path()),
                (root.join("sub/b.jpg").as_path(), root.join("Images/b.jpg").as_path()),
            ]
        );
        assert_eq!(plan.entries[2].action, Action::Unclassified);
    }

    #[test]
    fn existing_destination_is_a_collision() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::create_dir(root.join("Images")).unwrap();
        fs::write(root.join("Images/a.jpg"), "existing").unwrap();
        let plan = build_plan(root, &[classified(root.join("a.jpg"), "Images")]).unwrap();
        assert!(matches!(
            &plan.entries[0].action,
            Action::Collision { reason, .. } if reason == "destination already exists"
        ));
        assert_eq!(plan.moves().count(), 0);
    }

    #[test]
    fn two_files_with_same_destination_collide() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let plan = build_plan(
            root,
            &[
                classified(root.join("a.jpg"), "Images"),
                classified(root.join("sub/a.jpg"), "Images"),
            ],
        )
        .unwrap();
        assert!(matches!(plan.entries[0].action, Action::Move { .. }));
        assert!(matches!(
            &plan.entries[1].action,
            Action::Collision { reason, .. } if reason.contains("same destination")
        ));
    }

    #[test]
    fn file_already_in_its_category_folder_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let plan = build_plan(root, &[classified(root.join("Images/a.jpg"), "Images")]).unwrap();
        assert_eq!(
            plan.entries[0].action,
            Action::AlreadyInPlace {
                category: "Images".into()
            }
        );
    }

    #[test]
    fn category_names_that_escape_the_root_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for bad in [
            "..", ".", "../x", "a/../b", "a/./b", "/abs", "a/", "a//b", "a\\b", "",
        ] {
            let err = build_plan(root, &[classified(root.join("a.jpg"), bad)]).unwrap_err();
            assert!(matches!(err, PlanError::BadCategoryName(_)), "{bad:?}");
        }
    }

    #[test]
    fn nested_category_name_plans_under_both_levels() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let plan =
            build_plan(root, &[classified(root.join("a.jpg"), "工作/專案A")]).unwrap();
        let moves: Vec<_> = plan.moves().collect();
        assert_eq!(
            moves,
            vec![(
                root.join("a.jpg").as_path(),
                root.join("工作/專案A/a.jpg").as_path()
            )]
        );
        let plan = build_plan(
            root,
            &[classified(root.join("工作/專案A/a.jpg"), "工作/專案A")],
        )
        .unwrap();
        assert!(matches!(plan.entries[0].action, Action::AlreadyInPlace { .. }));
    }
}
