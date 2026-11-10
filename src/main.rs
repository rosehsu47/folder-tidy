use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use folder_tidy::apply::{apply, undo};
use folder_tidy::classify::classify_all;
use folder_tidy::config::Config;
use folder_tidy::plan::{Plan, build_plan};
use folder_tidy::scan::scan;

/// Exit code when the command ran but some files were not moved
/// (collisions on apply, unreversible moves on undo).
const EXIT_PARTIAL: u8 = 2;

#[derive(Parser)]
#[command(
    name = "folder-tidy",
    about = "Organize files into category folders by rules"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct Source {
    /// Folder to scan
    dir: PathBuf,
    /// Rules file (TOML)
    #[arg(long)]
    config: PathBuf,
    /// Include files in subfolders
    #[arg(long)]
    recursive: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Scan a folder and print each file's category as JSON. Moves nothing.
    Scan {
        #[command(flatten)]
        source: Source,
    },
    /// Dry run: print the move plan as JSON. Moves nothing.
    Plan {
        #[command(flatten)]
        source: Source,
    },
    /// Move files into category folders. Requires --yes; never overwrites.
    Apply {
        #[command(flatten)]
        source: Source,
        /// Move log (JSON) to append this apply to; `undo` reads it
        #[arg(long)]
        log: PathBuf,
        /// Confirm the moves. Without it nothing is moved.
        #[arg(long)]
        yes: bool,
    },
    /// Reverse the most recent apply recorded in the move log.
    Undo {
        /// Move log written by `apply`
        #[arg(long)]
        log: PathBuf,
    },
    /// Start the local web UI on http://127.0.0.1:<port>/ (loopback only).
    Serve {
        /// Rules file (TOML); the UI's settings are saved back to it
        #[arg(long)]
        config: PathBuf,
        /// Move log used for apply / undo / history
        #[arg(long)]
        log: PathBuf,
        #[arg(long, default_value_t = 8737)]
        port: u16,
    },
}

fn serve(config: &Path, log: &Path, port: u16) -> Result<(String, ExitCode), String> {
    let state = folder_tidy::web::AppState::new(config, log)?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let listener = folder_tidy::web::bind(port)
            .await
            .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
        let addr = listener.local_addr().map_err(|e| e.to_string())?;
        eprintln!("folder-tidy UI: http://{addr}/  (Ctrl-C to stop)");
        folder_tidy::web::serve(listener, state)
            .await
            .map_err(|e| format!("server stopped: {e}"))
    })?;
    Ok((String::new(), ExitCode::SUCCESS))
}

fn make_plan(source: &Source) -> Result<Plan, String> {
    let cfg = Config::load(&source.config).map_err(|e| e.to_string())?;
    let root = canonical_dir(&source.dir)?;
    let files = scan(&root, source.recursive).map_err(|e| e.to_string())?;
    build_plan(&root, &classify_all(&cfg, &files)).map_err(|e| e.to_string())
}

/// Absolute path of the folder, so logged paths work from any directory.
fn canonical_dir(dir: &Path) -> Result<PathBuf, String> {
    let root = dir
        .canonicalize()
        .map_err(|_| format!("not a directory: {}", dir.display()))?;
    if !root.is_dir() {
        return Err(format!("not a directory: {}", dir.display()));
    }
    Ok(root)
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<String, String> {
    serde_json::to_string_pretty(value).map_err(|e| e.to_string())
}

fn run(command: Command) -> Result<(String, ExitCode), String> {
    match command {
        Command::Scan { source } => {
            let cfg = Config::load(&source.config).map_err(|e| e.to_string())?;
            let files = scan(&source.dir, source.recursive).map_err(|e| e.to_string())?;
            Ok((to_json(&classify_all(&cfg, &files))?, ExitCode::SUCCESS))
        }
        Command::Plan { source } => Ok((to_json(&make_plan(&source)?)?, ExitCode::SUCCESS)),
        Command::Apply { source, log, yes } => {
            if !yes {
                return Err(
                    "apply moves files and needs explicit confirmation: pass --yes (run `plan` to preview first)"
                        .into(),
                );
            }
            let plan = make_plan(&source)?;
            let now = chrono::Local::now().to_rfc3339();
            let report = apply(&plan, &log, now).map_err(|e| e.to_string())?;
            let code = if report.collisions.is_empty() {
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "warning: {} file(s) not moved because the destination already exists (nothing was overwritten)",
                    report.collisions.len()
                );
                ExitCode::from(EXIT_PARTIAL)
            };
            Ok((to_json(&report)?, code))
        }
        Command::Undo { log } => {
            let report = undo(&log).map_err(|e| e.to_string())?;
            let code = if report.failed.is_empty() {
                ExitCode::SUCCESS
            } else {
                eprintln!(
                    "warning: {} file(s) could not be moved back; they stay in the log",
                    report.failed.len()
                );
                ExitCode::from(EXIT_PARTIAL)
            };
            Ok((to_json(&report)?, code))
        }
        Command::Serve { config, log, port } => serve(&config, &log, port),
    }
}

fn main() -> ExitCode {
    match run(Cli::parse().command) {
        Ok((json, code)) => {
            println!("{json}");
            code
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
