//! Classify files into categories using the configured rules.
//! Files no rule matches are `Unclassified` — never guessed.

use std::path::PathBuf;

use chrono::NaiveDate;
use serde::Serialize;

use crate::config::{Category, Config, DateRule};

/// What the classifier needs to know about a file.
#[derive(Debug, Clone)]
pub struct FileInfo {
    pub path: PathBuf,
    /// Modified date in local time.
    pub modified: NaiveDate,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum Classification {
    Classified { category: String, rule: String },
    Unclassified,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClassifiedFile {
    pub path: PathBuf,
    #[serde(flatten)]
    pub classification: Classification,
}

/// Classifies against today's local date (relative date rules count days
/// back from the scan date).
pub fn classify(config: &Config, file: &FileInfo) -> Classification {
    classify_at(config, file, chrono::Local::now().date_naive())
}

/// Like [`classify`], with the scan date given, so tests can fix it.
pub fn classify_at(config: &Config, file: &FileInfo, today: NaiveDate) -> Classification {
    for category in &config.categories {
        if let Some(rule) = match_category(category, file, today) {
            return Classification::Classified {
                category: category.name.clone(),
                rule,
            };
        }
    }
    Classification::Unclassified
}

pub fn classify_all(config: &Config, files: &[FileInfo]) -> Vec<ClassifiedFile> {
    files
        .iter()
        .map(|f| ClassifiedFile {
            path: f.path.clone(),
            classification: classify(config, f),
        })
        .collect()
}

/// Returns a human-readable description of the first rule that matched.
fn match_category(category: &Category, file: &FileInfo, today: NaiveDate) -> Option<String> {
    let name = file.path.file_name()?.to_string_lossy().to_lowercase();

    if let Some(ext) = file.path.extension() {
        let ext = ext.to_string_lossy().to_lowercase();
        if category
            .extensions
            .iter()
            .any(|e| e.trim_start_matches('.').to_lowercase() == ext)
        {
            return Some(format!("extension: {ext}"));
        }
    }

    if let Some(k) = category
        .keywords
        .iter()
        .find(|k| name.contains(&k.to_lowercase()))
    {
        return Some(format!("keyword: {k}"));
    }

    if let Some(d) = &category.date
        && date_matches(d, file.modified, today)
    {
        return Some(format!("modified: {}", file.modified));
    }

    None
}

fn date_matches(rule: &DateRule, modified: NaiveDate, today: NaiveDate) -> bool {
    // N days before the scan date; a span past the calendar's start means
    // "since forever".
    let days_ago = |n: u32| {
        today
            .checked_sub_days(chrono::Days::new(n.into()))
            .unwrap_or(NaiveDate::MIN)
    };
    rule.after.is_none_or(|a| modified >= a)
        && rule.before.is_none_or(|b| modified < b)
        && rule.newer_than_days.is_none_or(|n| modified >= days_ago(n))
        && rule.older_than_days.is_none_or(|o| modified < days_ago(o))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(y: i32, m: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, day).unwrap()
    }

    fn file(name: &str) -> FileInfo {
        FileInfo {
            path: PathBuf::from("/fixture").join(name),
            modified: d(2025, 6, 1),
        }
    }

    fn cfg(text: &str) -> Config {
        Config::parse(text).unwrap()
    }

    fn category_of(c: &Config, f: &FileInfo) -> Option<String> {
        match classify(c, f) {
            Classification::Classified { category, .. } => Some(category),
            Classification::Unclassified => None,
        }
    }

    #[test]
    fn extension_rule_matches_case_insensitively_with_or_without_dot() {
        let c = cfg("[[category]]\nname = \"Images\"\nextensions = [\"jpg\", \".png\"]\n");
        assert_eq!(category_of(&c, &file("a.JPG")), Some("Images".into()));
        assert_eq!(category_of(&c, &file("b.png")), Some("Images".into()));
        assert_eq!(category_of(&c, &file("c.jpeg")), None);
        // An extension rule is not a substring rule on the name.
        assert_eq!(category_of(&c, &file("jpg")), None);
        assert_eq!(category_of(&c, &file("photo.jpg.txt")), None);
    }

    #[test]
    fn keyword_rule_matches_file_name_substring_case_insensitively() {
        let c = cfg("[[category]]\nname = \"Invoices\"\nkeywords = [\"Invoice\"]\n");
        assert_eq!(
            category_of(&c, &file("2025-invoice-acme.pdf")),
            Some("Invoices".into())
        );
        assert_eq!(
            category_of(&c, &file("INVOICE.txt")),
            Some("Invoices".into())
        );
        assert_eq!(category_of(&c, &file("receipt.pdf")), None);
    }

    #[test]
    fn disabled_keyword_does_not_match_until_re_enabled() {
        let mut c =
            cfg("[[category]]\nname = \"Invoices\"\ndisabled_keywords = [\"invoice\"]\n");
        assert_eq!(category_of(&c, &file("invoice-1.pdf")), None);
        // Re-enable: move it from disabled_keywords to keywords.
        let k = c.categories[0].disabled_keywords.remove(0);
        c.categories[0].keywords.push(k);
        assert_eq!(category_of(&c, &file("invoice-1.pdf")), Some("Invoices".into()));
    }

    #[test]
    fn keyword_rule_ignores_parent_directories() {
        let c = cfg("[[category]]\nname = \"Invoices\"\nkeywords = [\"invoice\"]\n");
        let f = FileInfo {
            path: PathBuf::from("/fixture/invoices/notes.txt"),
            modified: d(2025, 6, 1),
        };
        assert_eq!(category_of(&c, &f), None);
    }

    #[test]
    fn date_rule_before_is_exclusive_after_is_inclusive() {
        let c = cfg(
            "[[category]]\nname = \"2024\"\ndate = { after = \"2024-01-01\", before = \"2025-01-01\" }\n",
        );
        let at = |date| FileInfo {
            path: PathBuf::from("/fixture/x.bin"),
            modified: date,
        };
        assert_eq!(category_of(&c, &at(d(2024, 1, 1))), Some("2024".into()));
        assert_eq!(category_of(&c, &at(d(2024, 12, 31))), Some("2024".into()));
        assert_eq!(category_of(&c, &at(d(2025, 1, 1))), None);
        assert_eq!(category_of(&c, &at(d(2023, 12, 31))), None);
    }

    #[test]
    fn date_rule_with_single_bound() {
        let c = cfg("[[category]]\nname = \"Old\"\ndate = { before = \"2020-01-01\" }\n");
        let at = |date| FileInfo {
            path: PathBuf::from("/fixture/x.bin"),
            modified: date,
        };
        assert_eq!(category_of(&c, &at(d(2019, 5, 5))), Some("Old".into()));
        assert_eq!(category_of(&c, &at(d(2020, 1, 1))), None);
    }

    #[test]
    fn relative_date_presets_count_days_back_from_the_scan_date() {
        // T-042: 一週內 = newer_than_days 7, 超過一年 = older_than_days 365.
        let c = cfg(
            "[[category]]\nname = \"Recent\"\ndate = { newer_than_days = 7 }\n\n\
             [[category]]\nname = \"Old\"\ndate = { older_than_days = 365 }\n",
        );
        let today = d(2026, 11, 9);
        let at = |date| FileInfo {
            path: PathBuf::from("/fixture/x.bin"),
            modified: date,
        };
        let cat = |f: &FileInfo| match classify_at(&c, f, today) {
            Classification::Classified { category, .. } => Some(category),
            Classification::Unclassified => None,
        };
        assert_eq!(cat(&at(d(2026, 11, 6))), Some("Recent".into()), "3 days ago");
        assert_eq!(cat(&at(d(2026, 11, 2))), Some("Recent".into()), "exactly 7 days ago is within");
        assert_eq!(cat(&at(d(2026, 11, 1))), None, "8 days ago is neither");
        assert_eq!(cat(&at(d(2025, 10, 1))), Some("Old".into()), "more than a year ago");
        assert_eq!(cat(&at(d(2025, 11, 9))), None, "exactly 365 days ago is not more than a year");
    }

    #[test]
    fn relative_range_between_two_bounds() {
        // modified 8..30 days ago
        let c = cfg("[[category]]\nname = \"Mid\"\ndate = { newer_than_days = 30, older_than_days = 7 }\n");
        let today = d(2026, 11, 9);
        let at = |date| FileInfo { path: PathBuf::from("/fixture/x.bin"), modified: date };
        let hit = |f: &FileInfo| classify_at(&c, f, today) != Classification::Unclassified;
        assert!(hit(&at(d(2026, 10, 20))));
        assert!(!hit(&at(d(2026, 11, 5))), "too recent");
        assert!(!hit(&at(d(2026, 9, 1))), "too old");
    }

    #[test]
    fn first_matching_category_wins_and_rule_is_reported() {
        let c = cfg(
            "[[category]]\nname = \"Screenshots\"\nkeywords = [\"screenshot\"]\n\
             [[category]]\nname = \"Images\"\nextensions = [\"png\"]\n",
        );
        assert_eq!(
            classify(&c, &file("Screenshot 2025.png")),
            Classification::Classified {
                category: "Screenshots".into(),
                rule: "keyword: screenshot".into()
            }
        );
        assert_eq!(
            classify(&c, &file("logo.png")),
            Classification::Classified {
                category: "Images".into(),
                rule: "extension: png".into()
            }
        );
    }

    #[test]
    fn unmatched_file_is_unclassified() {
        let c = cfg("[[category]]\nname = \"Images\"\nextensions = [\"jpg\"]\n");
        assert_eq!(
            classify(&c, &file("mystery.xyz")),
            Classification::Unclassified
        );
        assert_eq!(
            classify(&c, &file("no_extension")),
            Classification::Unclassified
        );
        assert_eq!(
            classify(&Config::default(), &file("a.jpg")),
            Classification::Unclassified
        );
    }

    #[test]
    fn serializes_with_status_tag() {
        let c = cfg("[[category]]\nname = \"Images\"\nextensions = [\"jpg\"]\n");
        let out = classify_all(&c, &[file("a.jpg"), file("b.xyz")]);
        let json = serde_json::to_value(&out).unwrap();
        assert_eq!(
            json,
            serde_json::json!([
                {"path": "/fixture/a.jpg", "status": "classified", "category": "Images", "rule": "extension: jpg"},
                {"path": "/fixture/b.xyz", "status": "unclassified"}
            ])
        );
    }
}
