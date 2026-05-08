use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use gloss::{diff, list, restore, snapshot, CheckpointInfo, FileStatus};

#[derive(Parser)]
#[command(name = "gloss", version, about = "Out-of-tree git checkpoints for AI coding agents")]
struct Cli {
    #[arg(long, global = true, value_name = "PATH")]
    workspace: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Take a snapshot of the workspace.
    Snap {
        /// Optional human-readable label.
        label: Option<String>,
    },
    /// List checkpoints, newest first.
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Show diff for a checkpoint (defaults to the latest).
    Diff {
        /// Checkpoint id or unique prefix. Omit for the latest checkpoint.
        id: Option<String>,
    },
    /// Restore the workspace to a checkpoint.
    Restore {
        /// Checkpoint id or unique prefix.
        id: String,
    },
    /// Start the web UI on http://127.0.0.1:<port>.
    Serve {
        /// Preferred port. Falls back to next free port if busy.
        #[arg(long)]
        port: Option<u16>,
        /// Open the browser automatically.
        #[arg(long)]
        open: bool,
        /// Session label — shown as browser tab title and badge.
        /// Each distinct label gets its own deterministic port,
        /// so multiple Claude sessions can run side by side.
        #[arg(long)]
        label: Option<String>,
    },
    /// Open a review tab for the current Claude Code session.
    /// Equivalent to `serve --open --label <LABEL>` with a deterministic
    /// per-(workspace, label) port. Designed for `gloss review my-session`
    /// from inside a Claude Code conversation.
    Review {
        /// Session label. Defaults to a hash of pwd if omitted.
        label: Option<String>,
    },
    /// Run as an MCP server over stdio (for Claude Code / Claude Desktop / Cursor).
    Mcp,
    /// One-shot wire-up into Claude Code: register MCP, add hooks, install slash command.
    Install,
    /// Reverse `install`.
    Uninstall,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let ws = match cli.workspace {
        Some(p) => p,
        None => match std::env::current_dir() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("cannot read current dir: {e}");
                return ExitCode::from(2);
            }
        },
    };

    let result = match cli.cmd {
        Cmd::Snap { label } => cmd_snap(&ws, label).await,
        Cmd::List { limit } => cmd_list(&ws, limit),
        Cmd::Diff { id } => cmd_diff(&ws, id).await,
        Cmd::Restore { id } => cmd_restore(&ws, &id).await,
        Cmd::Serve { port, open, label } => cmd_serve(ws.clone(), port, open, label).await,
        Cmd::Review { label } => cmd_review(ws.clone(), label).await,
        Cmd::Mcp => gloss::mcp::run(ws.clone()).await,
        Cmd::Install => gloss::install::install().map(|r| {
            gloss::install::print_report("install", &r);
        }),
        Cmd::Uninstall => gloss::install::uninstall().map(|r| {
            gloss::install::print_report("uninstall", &r);
        }),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::from(1)
        }
    }
}

async fn cmd_snap(ws: &std::path::Path, label: Option<String>) -> Result<(), String> {
    let cp = snapshot(ws, label, None).await?;
    println!("{}  ({} files, +{} -{})", cp.id, cp.files_changed, cp.insertions, cp.deletions);
    if let Some(l) = &cp.label {
        println!("  label: {l}");
    }
    Ok(())
}

fn cmd_list(ws: &std::path::Path, limit: usize) -> Result<(), String> {
    let items = list(ws);
    if items.is_empty() {
        println!("(no checkpoints)");
        return Ok(());
    }
    for cp in items.iter().take(limit) {
        let when = format_ts(cp.created_at_ms);
        let label = cp.label.as_deref().unwrap_or("-");
        println!(
            "{}  {}  {:>3} files  +{:<5} -{:<5}  {}",
            cp.id, when, cp.files_changed, cp.insertions, cp.deletions, label
        );
    }
    Ok(())
}

async fn cmd_diff(ws: &std::path::Path, id: Option<String>) -> Result<(), String> {
    let target_id = match id {
        Some(s) => resolve_id(ws, &s)?,
        None => list(ws)
            .into_iter()
            .next()
            .map(|c| c.id)
            .ok_or_else(|| "no checkpoints yet — run `gloss snap` first".to_string())?,
    };

    let diffs = diff(ws, &target_id).await?;
    if diffs.is_empty() {
        println!("(no changes)");
        return Ok(());
    }

    let use_color = is_terminal(std::io::stdout());
    for fd in &diffs {
        print_file_diff(fd, use_color);
    }
    Ok(())
}

async fn cmd_serve(
    ws: PathBuf,
    port: Option<u16>,
    open: bool,
    label: Option<String>,
) -> Result<(), String> {
    let port = port.unwrap_or_else(|| gloss::web::port_for(&ws, label.as_deref()));
    if open {
        spawn_browser(port);
    }
    gloss::web::serve(ws, port, label).await
}

async fn cmd_review(ws: PathBuf, label: Option<String>) -> Result<(), String> {
    let port = gloss::web::port_for(&ws, label.as_deref());
    spawn_browser(port);
    // If something is already serving on the deterministic port (almost
    // certainly a previous gloss from another Claude session in the
    // same workspace+label), skip starting a second server. The browser
    // will land on the existing one. This makes `gloss review`
    // idempotent and safe to call from SessionStart hooks.
    if tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .is_ok()
    {
        println!("gloss already running at http://127.0.0.1:{port}");
        return Ok(());
    }
    gloss::web::serve(ws, port, label).await
}

fn spawn_browser(port: u16) {
    let url = format!("http://127.0.0.1:{port}");
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(target_os = "windows") {
        "explorer"
    } else {
        "xdg-open"
    };
    // Delay so the server has a moment to bind before the browser hits it.
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let _ = std::process::Command::new(cmd).arg(&url).spawn();
    });
}

async fn cmd_restore(ws: &std::path::Path, id: &str) -> Result<(), String> {
    let resolved = resolve_id(ws, id)?;
    let report = restore(ws, &resolved, None).await?;
    println!("restored {} files, removed {} files", report.restored_files.len(), report.removed_files.len());
    if let Some(stash) = report.stash_commit {
        println!("pre-restore working-tree captured at stash commit {stash}");
    }
    Ok(())
}

fn resolve_id(ws: &std::path::Path, prefix: &str) -> Result<String, String> {
    let items = list(ws);
    let matches: Vec<&CheckpointInfo> = items.iter().filter(|c| c.id.starts_with(prefix)).collect();
    match matches.len() {
        0 => Err(format!("no checkpoint matches `{prefix}`")),
        1 => Ok(matches[0].id.clone()),
        _ => Err(format!(
            "ambiguous prefix `{prefix}` matches {} checkpoints",
            matches.len()
        )),
    }
}

fn format_ts(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let delta = now - secs;
    if delta < 60 {
        format!("{}s ago", delta.max(0))
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

fn is_terminal<S: std::os::fd::AsFd>(s: S) -> bool {
    use std::os::fd::AsRawFd;
    let fd = s.as_fd().as_raw_fd();
    unsafe { libc_isatty(fd) }
}

#[cfg(unix)]
unsafe fn libc_isatty(fd: i32) -> bool {
    extern "C" {
        fn isatty(fd: i32) -> i32;
    }
    isatty(fd) == 1
}

#[cfg(not(unix))]
unsafe fn libc_isatty(_fd: i32) -> bool {
    false
}

fn print_file_diff(fd: &gloss::FileDiff, color: bool) {
    let (reset, bold, cyan, green, red, dim) = if color {
        ("\x1b[0m", "\x1b[1m", "\x1b[36m", "\x1b[32m", "\x1b[31m", "\x1b[2m")
    } else {
        ("", "", "", "", "", "")
    };

    let status = match fd.status {
        FileStatus::Added => "added",
        FileStatus::Modified => "modified",
        FileStatus::Deleted => "deleted",
        FileStatus::Renamed => "renamed",
        FileStatus::Copied => "copied",
    };

    println!(
        "\n{bold}{cyan}── {} ──{reset}  {dim}{}  +{} -{}{}{reset}",
        fd.path,
        status,
        fd.additions,
        fd.deletions,
        if fd.truncated { "  (truncated)" } else { "" },
    );

    if fd.patch.is_empty() {
        println!("{dim}(no textual diff: binary or rename-only){reset}");
        return;
    }

    for line in fd.patch.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            println!("{bold}{}{reset}", line);
        } else if line.starts_with('+') {
            println!("{green}{}{reset}", line);
        } else if line.starts_with('-') {
            println!("{red}{}{reset}", line);
        } else if line.starts_with("@@") {
            println!("{cyan}{}{reset}", line);
        } else {
            println!("{}", line);
        }
    }
}
