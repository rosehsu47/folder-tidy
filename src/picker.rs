//! Native folder picker for the 來源 screen's 選擇… button.
//!
//! A browser cannot hand a page the absolute path of a folder the user picks
//! (`<input webkitdirectory>` only exposes relative file names), so the
//! server opens the macOS `choose folder` dialog itself via `osascript` and
//! returns the POSIX path. The server only binds 127.0.0.1, so the dialog
//! opens on the same Mac as the browser.

use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;

pub trait FolderPicker: Send + Sync {
    /// Shows the dialog, starting in `start` when given. `Ok(None)` means the
    /// user pressed Cancel.
    fn pick(&self, start: Option<&Path>) -> Result<Option<PathBuf>, PickError>;
}

#[derive(Debug)]
pub enum PickError {
    Spawn(std::io::Error),
    /// osascript exited non-zero for a reason other than Cancel.
    Failed { status: String, stderr: String },
    InvalidOutput(String),
}

impl fmt::Display for PickError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PickError::Spawn(e) => write!(f, "could not open the folder picker (osascript): {e}; type the path instead"),
            PickError::Failed { status, stderr } => match stderr.trim() {
                "" => write!(f, "the folder picker failed ({status}); type the path instead"),
                msg => write!(f, "the folder picker failed ({status}): {msg}; type the path instead"),
            },
            PickError::InvalidOutput(s) => write!(f, "the folder picker returned an unexpected answer: {s:?}"),
        }
    }
}

/// AppleScript run with the start folder (or nothing) as its only argument,
/// so the path is never spliced into the script text.
const SCRIPT: &str = r#"on run argv
  activate
  if (count of argv) > 0 then
    set picked to choose folder with prompt "選擇要整理的資料夾" default location (POSIX file (item 1 of argv))
  else
    set picked to choose folder with prompt "選擇要整理的資料夾"
  end if
  return POSIX path of picked
end run"#;

/// AppleScript's "User canceled." error number, printed in stderr.
const CANCELED: &str = "(-128)";

#[derive(Default)]
pub struct Osascript;

impl FolderPicker for Osascript {
    fn pick(&self, start: Option<&Path>) -> Result<Option<PathBuf>, PickError> {
        let mut cmd = Command::new("/usr/bin/osascript");
        cmd.arg("-e").arg(SCRIPT);
        if let Some(dir) = start {
            cmd.arg(dir);
        }
        let out = cmd.output().map_err(PickError::Spawn)?;
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        if !out.status.success() {
            if stderr.contains(CANCELED) {
                return Ok(None);
            }
            return Err(PickError::Failed {
                status: out.status.to_string(),
                stderr,
            });
        }
        let stdout = String::from_utf8(out.stdout)
            .map_err(|e| PickError::InvalidOutput(String::from_utf8_lossy(e.as_bytes()).into_owned()))?;
        parse_output(&stdout).map(Some)
    }
}

/// `POSIX path of` ends a folder with `/` and osascript adds a newline.
fn parse_output(stdout: &str) -> Result<PathBuf, PickError> {
    let line = stdout.strip_suffix('\n').unwrap_or(stdout);
    if !line.starts_with('/') || line.contains('\n') {
        return Err(PickError::InvalidOutput(stdout.to_owned()));
    }
    let trimmed = if line.len() > 1 { line.strip_suffix('/').unwrap_or(line) } else { line };
    Ok(PathBuf::from(trimmed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_output_drops_the_trailing_slash_and_newline() {
        assert_eq!(parse_output("/Users/me/Desktop/\n").unwrap(), PathBuf::from("/Users/me/Desktop"));
        assert_eq!(parse_output("/\n").unwrap(), PathBuf::from("/"));
        assert_eq!(parse_output("/Volumes/My Disk/截圖/\n").unwrap(), PathBuf::from("/Volumes/My Disk/截圖"));
    }

    #[test]
    fn parse_output_rejects_anything_but_one_absolute_path() {
        assert!(parse_output("").is_err());
        assert!(parse_output("Macintosh HD:Users:\n").is_err());
        assert!(parse_output("/a/\n/b/\n").is_err());
    }
}
