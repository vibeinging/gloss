use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Path as AxPath, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect};
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;

use crate::{diff, list, restore, CheckpointInfo, FileDiff, FileStatus};

#[derive(Clone)]
struct AppState {
    workspace: PathBuf,
    label: Option<String>,
}

#[derive(Clone)]
struct AppStateFull {
    workspace: PathBuf,
    label: Option<String>,
}

pub async fn serve(
    workspace: PathBuf,
    preferred_port: u16,
    label: Option<String>,
) -> Result<(), String> {
    let state = AppStateFull { workspace: workspace.clone(), label: label.clone() };

    let (listener, addr) = bind_with_fallback(preferred_port).await?;
    let app = Router::new()
        .route("/", get(index))
        .route("/restore/:id", post(post_restore))
        .with_state(Arc::new(AppState {
            workspace: state.workspace.clone(),
            label: state.label.clone(),
        }));

    if let Some(l) = &label {
        println!("gloss [{}] http://{}", l, addr);
    } else {
        println!("gloss http://{}", addr);
    }
    println!("workspace: {}", workspace.display());
    axum::serve(listener, app)
        .await
        .map_err(|e| format!("serve: {e}"))?;
    Ok(())
}

async fn bind_with_fallback(preferred: u16) -> Result<(tokio::net::TcpListener, String), String> {
    for offset in 0..20u16 {
        let port = preferred.saturating_add(offset);
        let addr = format!("127.0.0.1:{port}");
        if let Ok(l) = tokio::net::TcpListener::bind(&addr).await {
            return Ok((l, addr));
        }
    }
    let l = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| format!("bind any port: {e}"))?;
    let addr = l.local_addr().map_err(|e| e.to_string())?.to_string();
    Ok((l, addr))
}

/// Deterministic port per workspace. Label is intentionally NOT mixed in:
/// every Claude session for the same project shares one server + one
/// browser tab; sessions are surfaced as in-page tabs. The `_label` arg
/// is retained for API compatibility. Range: 7400–7999.
pub fn port_for(workspace: &std::path::Path, _label: Option<&str>) -> u16 {
    let mut h: u64 = 0xcbf29ce484222325;
    let canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    for b in canon.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    7400 + ((h % 600) as u16)
}

#[derive(Deserialize)]
struct IndexQuery {
    id: Option<String>,
    msg: Option<String>,
    /// Session index, 0 = newest. Sessions are derived by splitting the
    /// flat checkpoint timeline on idle gaps > SESSION_GAP_MS.
    session: Option<usize>,
}

/// 30 minutes of inactivity = a new session.
const SESSION_GAP_MS: u64 = 30 * 60 * 1000;

/// Group a newest-first checkpoint list into sessions. Each inner Vec is
/// itself newest-first within that session.
fn group_sessions(cps: &[CheckpointInfo]) -> Vec<Vec<&CheckpointInfo>> {
    let mut out: Vec<Vec<&CheckpointInfo>> = Vec::new();
    for cp in cps {
        let needs_new = match out.last() {
            None => true,
            Some(last) => {
                let prev = last.last().unwrap();
                prev.created_at_ms.saturating_sub(cp.created_at_ms) > SESSION_GAP_MS
            }
        };
        if needs_new {
            out.push(vec![cp]);
        } else {
            out.last_mut().unwrap().push(cp);
        }
    }
    out
}

async fn index(
    State(state): State<Arc<AppState>>,
    Query(q): Query<IndexQuery>,
) -> Result<Html<String>, AppError> {
    let ws = state.workspace.clone();
    let checkpoints = tokio::task::spawn_blocking(move || list(&ws))
        .await
        .map_err(|e| AppError(format!("join: {e}")))?;

    let sessions = group_sessions(&checkpoints);
    let max_idx = sessions.len().saturating_sub(1);
    let session_idx = q.session.unwrap_or(0).min(max_idx);
    let visible: &[&CheckpointInfo] = sessions
        .get(session_idx)
        .map(|v| v.as_slice())
        .unwrap_or(&[]);

    // Resolve the selected checkpoint: respect ?id= if it falls inside the
    // visible session, else default to the newest in that session.
    let selected_id = q
        .id
        .clone()
        .filter(|id| visible.iter().any(|c| &c.id == id))
        .or_else(|| visible.first().map(|c| c.id.clone()));

    let diffs = if let Some(id) = &selected_id {
        match diff(&state.workspace, id).await {
            Ok(d) => Some(d),
            Err(_) => None,
        }
    } else {
        None
    };

    Ok(Html(render_page(
        &state.workspace,
        state.label.as_deref(),
        &sessions,
        session_idx,
        selected_id.as_deref(),
        diffs.as_deref(),
        q.msg.as_deref(),
    )))
}

async fn post_restore(
    State(state): State<Arc<AppState>>,
    AxPath(id): AxPath<String>,
) -> Result<Redirect, AppError> {
    let report = restore(&state.workspace, &id, None)
        .await
        .map_err(AppError)?;
    let msg = format!(
        "restored {} files, removed {} files{}",
        report.restored_files.len(),
        report.removed_files.len(),
        report
            .stash_commit
            .as_deref()
            .map(|s| format!(" (pre-restore stashed at {})", &s[..s.len().min(8)]))
            .unwrap_or_default(),
    );
    Ok(Redirect::to(&format!(
        "/?id={}&msg={}",
        urlencode(&id),
        urlencode(&msg)
    )))
}

struct AppError(String);

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0).into_response()
    }
}

fn render_page(
    workspace: &std::path::Path,
    label: Option<&str>,
    sessions: &[Vec<&CheckpointInfo>],
    session_idx: usize,
    selected: Option<&str>,
    diffs: Option<&[FileDiff]>,
    flash: Option<&str>,
) -> String {
    let mut out = String::new();
    let title = match label {
        Some(l) => format!("{} · gloss", l),
        None => "gloss".to_string(),
    };
    out.push_str(&format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{}</title>",
        esc(&title)
    ));
    out.push_str("<style>");
    out.push_str(CSS);
    out.push_str("</style></head><body>");

    if let Some(m) = flash {
        out.push_str(&format!("<div class=\"flash\">{}</div>", esc(m)));
    }

    out.push_str("<div class=\"layout\">");

    out.push_str("<aside>");

    // Project identity — replaces the old top-spanning header.
    let basename = workspace
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(".");
    let full = workspace.display().to_string();
    let pretty_path = dirs::home_dir()
        .and_then(|h| h.to_str().map(String::from))
        .filter(|h| full.starts_with(h))
        .map(|h| format!("~{}", &full[h.len()..]))
        .unwrap_or_else(|| full.clone());
    out.push_str(&format!(
        "<div class=\"project\" title=\"{}\"><div class=\"proj-name\">gloss · {}</div><div class=\"proj-path\">{}</div></div>",
        esc(&full),
        esc(basename),
        esc(&pretty_path),
    ));
    let _ = label; // browser tab <title> still uses it; no in-page badge any more.

    // Session tabs at the top of the sidebar. Each tab represents a burst
    // of activity (>30min idle = new session). Click to filter the
    // checkpoint list below.
    if !sessions.is_empty() {
        out.push_str("<nav class=\"session-tabs\" title=\"sessions split by 30min idle gaps\">");
        for (i, sess) in sessions.iter().enumerate() {
            let active = if i == session_idx { " active" } else { "" };
            let head = sess.first().unwrap();
            let when = format_ts(head.created_at_ms);
            // Prefer a meaningful label (anything other than auto turn-* tags)
            // but fall back to the relative time.
            let pretty_label = sess
                .iter()
                .filter_map(|c| c.label.as_deref())
                .find(|l| !l.starts_with("turn-"))
                .unwrap_or(&when);
            let count = sess.len();
            out.push_str(&format!(
                "<a class=\"session-tab{active}\" href=\"/?session={i}\" title=\"{count} checkpoints\">\
                <span class=\"st-label\">{label}</span>\
                <span class=\"st-meta\">{when} · {count}</span>\
                </a>",
                active = active,
                i = i,
                count = count,
                label = esc(pretty_label),
                when = esc(&when),
            ));
        }
        out.push_str("</nav>");
    }

    out.push_str("<div class=\"cps-wrap\"><h2>checkpoints</h2>");
    let visible = sessions.get(session_idx).map(|v| v.as_slice()).unwrap_or(&[]);
    if visible.is_empty() {
        out.push_str("<p class=\"empty\">no checkpoints yet — run <code>gloss snap</code></p>");
    } else {
        out.push_str("<ul class=\"cps\">");
        for cp in visible {
            let active = selected == Some(&cp.id);
            let cls = if active { "cp active" } else { "cp" };
            let cp_label = cp.label.as_deref().unwrap_or("(auto)");
            let when = format_ts(cp.created_at_ms);
            let short = &cp.id[..cp.id.len().min(20)];
            out.push_str(&format!(
                "<li class=\"{cls}\"><a href=\"/?session={s}&id={id}\"><div class=\"cp-label\">{label}</div>\
                <div class=\"cp-meta\">{when} · {fc} files · <span class=\"add\">+{ins}</span> <span class=\"del\">-{dels}</span></div>\
                <div class=\"cp-id\">{short}</div></a>\
                <form method=\"POST\" action=\"/restore/{id}\" onsubmit=\"return confirm('Restore workspace to this checkpoint? Current state will be auto-stashed.')\">\
                <button class=\"restore\" title=\"Restore workspace to this checkpoint\">↶ restore</button></form></li>",
                cls = cls,
                s = session_idx,
                id = esc(&cp.id),
                label = esc(cp_label),
                when = esc(&when),
                fc = cp.files_changed,
                ins = cp.insertions,
                dels = cp.deletions,
                short = esc(short),
            ));
        }
        out.push_str("</ul>");
    }
    out.push_str("</div></aside>");

    out.push_str("<main>");
    if let Some(diffs) = diffs {
        if diffs.is_empty() {
            out.push_str("<p class=\"empty\">(no changes in this checkpoint)</p>");
        } else {
            out.push_str(&format!(
                "<div class=\"diff-summary\">{} file{}</div>",
                diffs.len(),
                if diffs.len() == 1 { "" } else { "s" }
            ));
            for fd in diffs {
                out.push_str(&render_file_diff(fd));
            }
        }
    } else if sessions.is_empty() {
        out.push_str("<p class=\"empty\">Take your first snapshot:<br><code>gloss snap \"before-claude\"</code></p>");
    } else {
        out.push_str("<p class=\"empty\">Select a checkpoint on the left.</p>");
    }
    out.push_str("</main>");

    out.push_str("</div></body></html>");
    out
}

fn render_file_diff(fd: &FileDiff) -> String {
    let status = match fd.status {
        FileStatus::Added => ("added", "added"),
        FileStatus::Modified => ("modified", "modified"),
        FileStatus::Deleted => ("deleted", "deleted"),
        FileStatus::Renamed => ("renamed", "renamed"),
        FileStatus::Copied => ("copied", "copied"),
    };

    let mut s = String::new();
    s.push_str(&format!(
        "<details open class=\"file\"><summary><span class=\"status {cls}\">{label}</span> <span class=\"path\">{path}</span> <span class=\"stats\"><span class=\"add\">+{a}</span> <span class=\"del\">-{d}</span></span>{trunc}</summary>",
        cls = status.0,
        label = status.1,
        path = esc(&fd.path),
        a = fd.additions,
        d = fd.deletions,
        trunc = if fd.truncated { " <span class=\"trunc\">(truncated)</span>" } else { "" },
    ));

    if fd.patch.is_empty() {
        s.push_str("<div class=\"nodiff\">(no textual diff: binary, rename-only, or empty)</div>");
    } else {
        s.push_str("<pre class=\"patch\">");
        for line in fd.patch.lines() {
            let cls = if line.starts_with("+++") || line.starts_with("---") {
                "header"
            } else if line.starts_with("@@") {
                "hunk"
            } else if line.starts_with('+') {
                "add"
            } else if line.starts_with('-') {
                "del"
            } else if line.starts_with("diff ") || line.starts_with("index ") || line.starts_with("new file") || line.starts_with("deleted file") {
                "header"
            } else {
                "ctx"
            };
            s.push_str(&format!("<span class=\"{cls}\">{}</span>\n", esc(line)));
        }
        s.push_str("</pre>");
    }
    s.push_str("</details>");
    s
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

fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

const CSS: &str = r#"
* { box-sizing: border-box; }
body { margin: 0; font: 14px/1.5 -apple-system, BlinkMacSystemFont, "Segoe UI", system-ui, sans-serif; color: #1f2328; background: #f6f8fa; }
.flash { padding: 10px 20px; background: #ddf4ff; border-bottom: 1px solid #b6e3ff; color: #0969da; }
.layout { display: grid; grid-template-columns: 320px 1fr; height: 100vh; }
.project { padding: 14px 14px 12px; border-bottom: 1px solid #d1d9e0; flex-shrink: 0; }
.proj-name { font-weight: 700; font-size: 14px; color: #1f2328; }
.proj-path { font: 11px ui-monospace, SFMono-Regular, Menlo, monospace; color: #8c959f; margin-top: 3px; word-break: break-all; }
aside { background: #fff; border-right: 1px solid #d1d9e0; overflow-y: auto; display: flex; flex-direction: column; }
aside h2 { margin: 8px; font-size: 11px; text-transform: uppercase; letter-spacing: 0.5px; color: #59636e; }
.session-tabs { display: flex; flex-direction: column; padding: 6px; gap: 3px; border-bottom: 1px solid #d1d9e0; flex-shrink: 0; max-height: 30vh; overflow-y: auto; }
.session-tab { display: flex; flex-direction: column; padding: 7px 10px; border-radius: 5px; background: transparent; font-size: 12px; text-decoration: none; color: #1f2328; border-left: 3px solid transparent; line-height: 1.3; }
.session-tab:hover { background: #f6f8fa; }
.session-tab.active { background: #ddf4ff; border-left-color: #0969da; color: #0969da; }
.session-tab .st-label { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.session-tab .st-meta { color: #59636e; font-size: 11px; }
.session-tab.active .st-meta { color: #0969da; }
.cps-wrap { padding: 8px; flex: 1; overflow-y: auto; }
.cps { list-style: none; margin: 0; padding: 0; }
.cp { position: relative; }
.cp a { display: block; padding: 10px 12px; border-radius: 6px; color: inherit; text-decoration: none; border: 1px solid transparent; }
.cp a:hover { background: #f6f8fa; }
.cp.active a { background: #ddf4ff; border-color: #b6e3ff; }
.cp-label { font-weight: 600; margin-bottom: 2px; }
.cp-meta { font-size: 12px; color: #59636e; }
.cp-id { font: 10px ui-monospace, SFMono-Regular, Menlo, monospace; color: #8c959f; margin-top: 2px; }
.cp form { display: inline; }
.cp .restore { position: absolute; top: 8px; right: 8px; opacity: 0; background: #fff; border: 1px solid #d1d9e0; border-radius: 4px; padding: 3px 8px; font-size: 11px; cursor: pointer; color: #59636e; }
.cp:hover .restore { opacity: 1; }
.cp .restore:hover { background: #cf222e; color: #fff; border-color: #cf222e; }
main { overflow-y: auto; padding: 16px 24px; }
.empty { color: #59636e; padding: 24px; text-align: center; }
.empty code { background: #eaeef2; padding: 2px 6px; border-radius: 4px; font-size: 13px; }
.diff-summary { color: #59636e; margin-bottom: 12px; font-size: 13px; }
.file { background: #fff; border: 1px solid #d1d9e0; border-radius: 6px; margin-bottom: 12px; overflow: hidden; }
.file summary { padding: 10px 14px; background: #f6f8fa; border-bottom: 1px solid #d1d9e0; cursor: pointer; user-select: none; display: flex; align-items: center; gap: 10px; }
.file summary:hover { background: #eaeef2; }
.file[open] summary { border-bottom: 1px solid #d1d9e0; }
.status { font-size: 11px; padding: 2px 8px; border-radius: 10px; font-weight: 600; }
.status.added { background: #dafbe1; color: #1a7f37; }
.status.modified { background: #fff8c5; color: #9a6700; }
.status.deleted { background: #ffebe9; color: #cf222e; }
.status.renamed, .status.copied { background: #ddf4ff; color: #0969da; }
.path { font: 13px ui-monospace, SFMono-Regular, Menlo, monospace; flex: 1; }
.stats { font: 11px ui-monospace, SFMono-Regular, Menlo, monospace; }
.add { color: #1a7f37; }
.del { color: #cf222e; }
.trunc { color: #9a6700; font-size: 11px; margin-left: 6px; }
.patch { margin: 0; padding: 0; font: 12px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace; overflow-x: auto; }
.patch span { display: block; padding: 0 14px; white-space: pre; }
.patch .header { color: #8c959f; background: #f6f8fa; }
.patch .hunk { color: #59636e; background: #ddf4ff; }
.patch .add { background: #e6ffec; color: #1a7f37; }
.patch .del { background: #ffebe9; color: #cf222e; }
.patch .ctx { color: #1f2328; }
.nodiff { padding: 14px; color: #59636e; font-style: italic; font-size: 13px; }
"#;
