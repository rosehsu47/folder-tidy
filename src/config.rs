//! Rule configuration: an ordered list of categories, each with its rules.
//!
//! ```toml
//! [[category]]
//! name = "Images"
//! extensions = ["jpg", "png"]
//! keywords = ["screenshot"]
//!
//! [[category]]
//! name = "Archive"
//! date = { before = "2024-01-01" }
//! ```
//!
//! Matching semantics: a category matches a file when **any** of its rules
//! matches (extension OR keyword OR date). Categories are checked in file
//! order; the first match wins. Extensions and keywords compare
//! case-insensitively. `disabled_keywords` stay in the file but are never
//! matched. A category with no rules matches nothing; it is a
//! folder files can only be assigned to by hand (the web UI's 新增資料夾).
//!
//! `ai_exclude` lists folders and `*.ext` patterns that are never sent to
//! the AI classifier (matching rules in [`crate::ai::is_excluded`]; edited
//! in the web UI's 分類設定).

use std::fmt;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Folders and `*.ext` patterns never sent to the AI.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ai_exclude: Vec<String>,
    #[serde(rename = "category", default)]
    pub categories: Vec<Category>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Category {
    pub name: String,
    /// File extensions without the leading dot, e.g. `"jpg"`.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Substrings matched against the file name.
    #[serde(default)]
    pub keywords: Vec<String>,
    /// Keywords kept but switched off: never matched, listed in the web UI
    /// so they can be switched back on.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_keywords: Vec<String>,
    /// Modified-date rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub date: Option<DateRule>,
}

/// Matches files whose modified date (local time) is `>= after` and `< before`.
/// At least one bound must be set.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct DateRule {
    #[serde(
        default,
        deserialize_with = "de_opt_date",
        serialize_with = "ser_opt_date",
        skip_serializing_if = "Option::is_none"
    )]
    pub before: Option<NaiveDate>,
    #[serde(
        default,
        deserialize_with = "de_opt_date",
        serialize_with = "ser_opt_date",
        skip_serializing_if = "Option::is_none"
    )]
    pub after: Option<NaiveDate>,
    /// Relative form (D-006): modified within the last N days, counted from
    /// the scan date (`modified >= today - N`). 一週內 = 7, 一個月內 = 30.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub newer_than_days: Option<u32>,
    /// Relative form: modified more than N days before the scan date
    /// (`modified < today - N`). 超過一年 = 365.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub older_than_days: Option<u32>,
}

fn ser_opt_date<S>(date: &Option<NaiveDate>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match date {
        Some(d) => serializer.serialize_str(&d.format("%Y-%m-%d").to_string()),
        None => serializer.serialize_none(),
    }
}

/// Parses a `"YYYY-MM-DD"` string. chrono is built without its `serde`
/// feature (no dependency changes), so the date is read as a string here.
fn de_opt_date<'de, D>(deserializer: D) -> Result<Option<NaiveDate>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let text = String::deserialize(deserializer)?;
    NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .map(Some)
        .map_err(|e| {
            serde::de::Error::custom(format!("invalid date {text:?} (expected YYYY-MM-DD): {e}"))
        })
}

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
    Invalid(String),
    Serialize(toml::ser::Error),
    Write { path: PathBuf, source: std::io::Error },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "cannot read config: {e}"),
            ConfigError::Parse(e) => write!(f, "cannot parse config: {e}"),
            ConfigError::Invalid(msg) => write!(f, "invalid config: {msg}"),
            ConfigError::Serialize(e) => write!(f, "cannot serialize config: {e}"),
            ConfigError::Write { path, source } => {
                write!(f, "cannot write config {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Io)?;
        Config::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Config, ConfigError> {
        let config: Config = toml::from_str(text).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates, then writes the config as TOML via a temp file + rename so
    /// a crash never leaves a half-written file. Comments in the original
    /// file are not preserved.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        self.validate()?;
        let text = toml::to_string(self).map_err(ConfigError::Serialize)?;
        let write_err = |e| ConfigError::Write {
            path: path.to_path_buf(),
            source: e,
        };
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        std::fs::write(&tmp, text).map_err(write_err)?;
        std::fs::rename(&tmp, path).map_err(write_err)
    }

    pub fn category(&self, name: &str) -> Option<&Category> {
        self.categories.iter().find(|c| c.name == name)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.ai_exclude.iter().any(|e| e.trim().is_empty()) {
            return Err(ConfigError::Invalid("empty ai_exclude entry".into()));
        }
        let mut seen = std::collections::HashSet::new();
        for c in &self.categories {
            if c.name.trim().is_empty() {
                return Err(ConfigError::Invalid("category with empty name".into()));
            }
            crate::plan::check_category_name(&c.name)
                .map_err(|e| ConfigError::Invalid(e.to_string()))?;
            if !seen.insert(c.name.as_str()) {
                return Err(ConfigError::Invalid(format!(
                    "duplicate category name {:?}",
                    c.name
                )));
            }
            if c.extensions
                .iter()
                .any(|e| e.trim_start_matches('.').is_empty())
            {
                return Err(ConfigError::Invalid(format!(
                    "category {:?} has an empty extension",
                    c.name
                )));
            }
            if c.keywords.iter().chain(&c.disabled_keywords).any(|k| k.is_empty()) {
                return Err(ConfigError::Invalid(format!(
                    "category {:?} has an empty keyword",
                    c.name
                )));
            }
            // A keyword is either on or off. Matching is case-insensitive, so
            // the overlap check is too.
            if let Some(k) = c.keywords.iter().find(|k| {
                c.disabled_keywords
                    .iter()
                    .any(|d| d.to_lowercase() == k.to_lowercase())
            }) {
                return Err(ConfigError::Invalid(format!(
                    "category {:?} lists keyword {k:?} in both keywords and disabled_keywords; keep it in one",
                    c.name
                )));
            }
            if let Some(d) = &c.date {
                let absolute = d.after.is_some() || d.before.is_some();
                let relative = d.newer_than_days.is_some() || d.older_than_days.is_some();
                if absolute && relative {
                    return Err(ConfigError::Invalid(format!(
                        "category {:?} date rule mixes absolute dates (after/before) with relative days (newer_than_days/older_than_days); use one or the other",
                        c.name
                    )));
                }
                if let (Some(n), Some(o)) = (d.newer_than_days, d.older_than_days)
                    && o >= n
                {
                    return Err(ConfigError::Invalid(format!(
                        "category {:?} date rule matches nothing: older_than_days ({o}) must be less than newer_than_days ({n})",
                        c.name
                    )));
                }
                match (d.after, d.before) {
                    (None, None) if !relative => {
                        return Err(ConfigError::Invalid(format!(
                            "category {:?} date rule needs `before` and/or `after`, or `newer_than_days` and/or `older_than_days`",
                            c.name
                        )));
                    }
                    (Some(a), Some(b)) if a >= b => {
                        return Err(ConfigError::Invalid(format!(
                            "category {:?} date rule has after ({a}) not earlier than before ({b})",
                            c.name
                        )));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_all_rule_kinds() {
        let c = Config::parse(
            r#"
            [[category]]
            name = "Images"
            extensions = ["jpg", ".PNG"]
            keywords = ["screenshot"]

            [[category]]
            name = "Archive"
            date = { before = "2024-01-01", after = "2020-01-01" }
            "#,
        )
        .unwrap();
        assert_eq!(c.categories.len(), 2);
        assert_eq!(c.categories[0].extensions, vec!["jpg", ".PNG"]);
        let d = c.categories[1].date.as_ref().unwrap();
        assert_eq!(d.before, NaiveDate::from_ymd_opt(2024, 1, 1));
        assert_eq!(d.after, NaiveDate::from_ymd_opt(2020, 1, 1));
    }

    #[test]
    fn category_without_rules_is_a_manual_only_folder() {
        let c = Config::parse("[[category]]\nname = \"Manual\"\n").unwrap();
        assert_eq!(c.categories[0].name, "Manual");
        assert!(c.categories[0].extensions.is_empty() && c.categories[0].date.is_none());
    }

    #[test]
    fn save_round_trips_through_toml() {
        let c = Config::parse(
            r#"
            ai_exclude = ["~/Documents/tax", "*.key"]

            [[category]]
            name = "文件"
            extensions = ["pdf"]
            keywords = ["invoice"]

            [[category]]
            name = "Archive"
            date = { before = "2024-01-01" }

            [[category]]
            name = "Manual"
            "#,
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        assert!(!dir.path().join("rules.toml.tmp").exists());
    }

    #[test]
    fn save_refuses_an_invalid_config_and_leaves_the_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, "# original\n").unwrap();
        let bad = Config {
            ai_exclude: vec![" ".into()],
            categories: vec![],
        };
        assert!(bad.save(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "# original\n");
    }

    #[test]
    fn rejects_duplicate_names() {
        let err = Config::parse(
            "[[category]]\nname = \"A\"\nkeywords=[\"x\"]\n[[category]]\nname = \"A\"\nkeywords=[\"y\"]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("duplicate"), "{err}");
    }

    #[test]
    fn rejects_empty_date_rule_and_inverted_range() {
        let err = Config::parse("[[category]]\nname = \"A\"\ndate = {}\n").unwrap_err();
        assert!(err.to_string().contains("before"), "{err}");
        let err = Config::parse(
            "[[category]]\nname = \"A\"\ndate = { after = \"2024-01-01\", before = \"2023-01-01\" }\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("not earlier"), "{err}");
    }

    #[test]
    fn relative_date_rule_validation() {
        // T-042: absolute and relative on one category is an error.
        let err = Config::parse(
            "[[category]]\nname = \"A\"\ndate = { after = \"2024-01-01\", newer_than_days = 7 }\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("mixes absolute dates"), "{err}");
        // An empty relative range is an error.
        let err = Config::parse(
            "[[category]]\nname = \"A\"\ndate = { newer_than_days = 7, older_than_days = 30 }\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("matches nothing"), "{err}");
        // Relative alone is valid and round-trips through save.
        let c = Config::parse("[[category]]\nname = \"A\"\ndate = { older_than_days = 365 }\n").unwrap();
        let d = c.categories[0].date.as_ref().unwrap();
        assert_eq!((d.older_than_days, d.newer_than_days, d.after, d.before), (Some(365), None, None, None));
        let text = toml::to_string(&c).unwrap();
        assert!(text.contains("older_than_days = 365") && !text.contains("newer_than_days"), "{text}");
    }

    #[test]
    fn rules_file_without_relative_fields_loads_unchanged() {
        let c = Config::parse("[[category]]\nname = \"A\"\ndate = { after = \"2024-01-01\" }\n").unwrap();
        let d = c.categories[0].date.as_ref().unwrap();
        assert_eq!((d.newer_than_days, d.older_than_days), (None, None));
        assert!(!toml::to_string(&c).unwrap().contains("_days"));
    }

    #[test]
    fn rejects_malformed_date() {
        let err = Config::parse("[[category]]\nname = \"A\"\ndate = { before = \"2024-13-01\" }\n")
            .unwrap_err();
        assert!(err.to_string().contains("expected YYYY-MM-DD"), "{err}");
    }

    #[test]
    fn category_names_may_nest_with_slash() {
        let c = Config::parse("[[category]]\nname = \"工作/專案A\"\nkeywords = [\"月報表\"]\n")
            .unwrap();
        assert_eq!(c.categories[0].name, "工作/專案A");
    }

    #[test]
    fn rejects_unsafe_category_names() {
        for (bad, why) in [
            ("a//b", "empty segment"),
            ("a/", "trailing slash"),
            ("/abs", "leading slash"),
            (".", "dot"),
            ("a/./b", "dot segment"),
            ("..", "dotdot"),
            ("../x", "dotdot segment"),
            ("a/../b", "inner dotdot"),
            ("a\\\\b", "backslash"),
        ] {
            let text = format!("[[category]]\nname = \"{bad}\"\nkeywords = [\"x\"]\n");
            let err = Config::parse(&text).unwrap_err();
            assert!(err.to_string().contains("cannot be used as a folder name"), "{why}: {err}");
        }
    }

    #[test]
    fn disabled_keywords_are_optional_and_round_trip() {
        let old = Config::parse("[[category]]\nname = \"A\"\nkeywords = [\"x\"]\n").unwrap();
        assert!(old.categories[0].disabled_keywords.is_empty());
        assert!(!toml::to_string(&old).unwrap().contains("disabled_keywords"));

        let c = Config::parse(
            "[[category]]\nname = \"A\"\nkeywords = [\"x\"]\ndisabled_keywords = [\"y\"]\n",
        )
        .unwrap();
        assert_eq!(c.categories[0].disabled_keywords, ["y"]);
        assert_eq!(Config::parse(&toml::to_string(&c).unwrap()).unwrap(), c);
        assert!(
            Config::parse("[[category]]\nname = \"A\"\ndisabled_keywords = [\"\"]\n").is_err()
        );
    }

    #[test]
    fn rejects_a_keyword_both_on_and_off() {
        // T-039, case-insensitive like matching.
        let err = Config::parse(
            "[[category]]\nname = \"A\"\nkeywords = [\"Invoice\", \"memo\"]\ndisabled_keywords = [\"invoice\"]\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("both keywords and disabled_keywords"), "{err}");
        assert!(err.to_string().contains("Invoice"), "{err}");
        // Different keywords in the two lists are fine.
        assert!(Config::parse("[[category]]\nname = \"A\"\nkeywords = [\"memo\"]\ndisabled_keywords = [\"invoice\"]\n").is_ok());
        // The same keyword in two different categories is fine.
        assert!(Config::parse(
            "[[category]]\nname = \"A\"\nkeywords = [\"x\"]\n\n[[category]]\nname = \"B\"\ndisabled_keywords = [\"x\"]\n"
        )
        .is_ok());
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(Config::parse("[[category]]\nname = \"A\"\nextension = [\"jpg\"]\n").is_err());
    }
}
