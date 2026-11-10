//! Local page for controlling terminal sessions already open in this Warp process.
//!
//! The page binds to loopback only. Keystrokes go through `TerminalView::write_to_pty`,
//! which is the same path the UI uses, so this bridge never locks `TerminalModel`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use warpui::{Entity, EntityId, ModelContext, ModelSpawner, SingletonEntity};

use crate::terminal::cli_agent_sessions::{CLIAgentSessionsModel, CLIAgentSessionsModelEvent};
use crate::terminal::view::TerminalView;
use crate::workspace::Workspace;

mod wechat;

const PAGE: &str = include_str!("remote_session_page.html");
const ICON: &[u8] = include_bytes!("../channels/oss/icon/no-padding/512x512.png");
const MAX_CONTENT_CHARS: usize = 24_000;
const PREVIEW_CHARS: usize = 160;

#[derive(Clone)]
struct BridgeState {
    spawner: ModelSpawner<RemoteSessionBridge>,
    token: Arc<str>,
    read_only: bool,
}

pub struct RemoteSessionBridge {
    /// Present only when the WeChat channel is enabled; agent status changes are
    /// queued here and delivered by the channel's own long-poll task.
    notifier: Option<UnboundedSender<String>>,
    /// Last pushed status tag per terminal, so a repeated status does not re-notify.
    last_pushed: HashMap<EntityId, &'static str>,
    /// `http://host:port/?token=…` prefix for deep links in WeChat pushes.
    page_url_base: String,
}

impl Entity for RemoteSessionBridge {
    type Event = ();
}

impl SingletonEntity for RemoteSessionBridge {}

impl RemoteSessionBridge {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let token = std::env::var("WARP_REMOTE_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .unwrap_or_else(load_or_create_token);
        let state = BridgeState {
            spawner: ctx.spawner(),
            token: Arc::from(token),
            read_only: read_only_mode(),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build();
        match runtime {
            Ok(runtime) => {
                runtime.spawn(serve(state.clone()));
                let notifier = wechat::enabled().then(|| {
                    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                    runtime.spawn(wechat::run(state.spawner.clone(), rx));
                    tx
                });
                if notifier.is_some() {
                    let sessions = CLIAgentSessionsModel::handle(ctx);
                    ctx.subscribe_to_model(&sessions, |bridge, _, event, ctx| {
                        bridge.on_cli_agent_event(event, ctx);
                    });
                }
                std::mem::forget(runtime);
                Self {
                    notifier,
                    last_pushed: HashMap::new(),
                    page_url_base: page_url_base(&state.token),
                }
            }
            Err(err) => {
                log::warn!("remote session bridge failed to start: {err}");
                Self {
                    notifier: None,
                    last_pushed: HashMap::new(),
                    page_url_base: String::new(),
                }
            }
        }
    }

    fn on_cli_agent_event(
        &mut self,
        event: &CLIAgentSessionsModelEvent,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(notifier) = self.notifier.clone() else {
            return;
        };
        match event {
            CLIAgentSessionsModelEvent::StatusChanged {
                terminal_view_id,
                agent,
                status,
                ..
            } => {
                // Record every status, not just pushed ones: otherwise
                // Blocked → InProgress → Blocked is deduped away and the
                // second approval request never reaches WeChat.
                let tag = wechat::status_tag(status);
                if self.last_pushed.get(terminal_view_id) == Some(&tag) {
                    return;
                }
                self.last_pushed.insert(*terminal_view_id, tag);
                let title = session_title(ctx, *terminal_view_id);
                let tail = push_tail(ctx, *terminal_view_id);
                let url = format!("{}&session={terminal_view_id}", self.page_url_base);
                let Some(text) =
                    wechat::status_notification(agent.display_name(), &title, status, &tail, &url)
                else {
                    return;
                };
                if notifier.send(text).is_err() {
                    log::debug!("wechat channel is not running; dropped an agent status push");
                }
            }
            CLIAgentSessionsModelEvent::Ended {
                terminal_view_id, ..
            } => {
                self.last_pushed.remove(terminal_view_id);
            }
            CLIAgentSessionsModelEvent::Started { .. }
            | CLIAgentSessionsModelEvent::InputSessionChanged { .. }
            | CLIAgentSessionsModelEvent::SessionUpdated { .. } => {}
        }
    }
}

fn session_title(ctx: &mut ModelContext<RemoteSessionBridge>, id: EntityId) -> String {
    for window_id in ctx.window_ids() {
        let Some(workspaces) = ctx.views_of_type::<Workspace>(window_id) else {
            continue;
        };
        for workspace in workspaces {
            let workspace_ref = workspace.as_ref(ctx);
            for (index, tab) in workspace_ref.tabs.iter().enumerate() {
                let pane_group = tab.pane_group.as_ref(ctx);
                for view in pane_group.visible_terminal_views(ctx) {
                    if view.id() == id {
                        let title = pane_group.display_title(ctx);
                        return format!("{title}（标签 {}）", index + 1);
                    }
                }
            }
        }
    }
    String::new()
}

/// When `WARP_REMOTE_READONLY=1`, the page can view output but every keystroke
/// endpoint is refused — a safe mode for sharing the URL more widely.
fn read_only_mode() -> bool {
    std::env::var("WARP_REMOTE_READONLY")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Base URL (with token) for deep links embedded in WeChat pushes.
///
/// `WARP_REMOTE_URL` wins because VPN adapters (蒲公英 etc.) are usually not
/// the primary outbound interface and cannot be auto-detected reliably.
fn page_url_base(token: &str) -> String {
    let base = std::env::var("WARP_REMOTE_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .unwrap_or_else(|| format!("http://{}:{}", primary_lan_ip(), bridge_port()));
    format!("{}/?token={token}", base.trim_end_matches('/'))
}

fn bridge_port() -> String {
    std::env::var("WARP_REMOTE_BIND")
        .ok()
        .and_then(|bind| bind.rsplit(':').next().map(str::to_owned))
        .filter(|port| !port.is_empty())
        .unwrap_or_else(|| "9283".to_owned())
}

/// Primary outbound IPv4 without sending packets; a UDP `connect` only picks
/// the route. 223.5.5.5 (AliDNS) is reachable from mainland networks.
fn primary_lan_ip() -> String {
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return "127.0.0.1".to_owned();
    };
    if socket.connect("223.5.5.5:80").is_err() {
        return "127.0.0.1".to_owned();
    }
    socket
        .local_addr()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_owned())
}

/// Host CPU/memory for the page's resource strip.
fn host_stats() -> Value {
    use sysinfo::System;
    static SYSTEM: std::sync::LazyLock<std::sync::Mutex<System>> =
        std::sync::LazyLock::new(|| std::sync::Mutex::new(System::new()));
    let mut system = SYSTEM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    system.refresh_memory();
    system.refresh_cpu_usage();
    json!({
        "cpu": system.global_cpu_usage().round().clamp(0., 100.) as i64,
        "memUsed": system.used_memory(),
        "memTotal": system.total_memory(),
    })
}

/// Reads the persisted page token, creating a random one on first run.
///
/// The page can read terminal output and send keystrokes, so a guessable
/// default token would expose any device on the same network.
fn load_or_create_token() -> String {
    let dir = std::env::var("APPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."))
        .join("warp-dev");
    let path = dir.join("remote-token.txt");
    if let Ok(token) = std::fs::read_to_string(&path) {
        let token = token.trim();
        if !token.is_empty() {
            return token.to_owned();
        }
    }
    let token: String = (0..32)
        .map(|_| {
            let byte = rand::random::<u8>();
            char::from_digit((byte % 16) as u32, 16).unwrap_or('0')
        })
        .collect();
    if std::fs::create_dir_all(&dir).is_ok() {
        let _ = std::fs::write(&path, format!("{token}\n"));
    }
    log::info!("remote session page token written to {}", path.display());
    token
}

async fn serve(state: BridgeState) {
    // Terminal text compresses several-fold, which matters on the slow VPN
    // links (蒲公英 etc.) this page is usually reached through.
    let router = axum::Router::new()
        .route("/", get(page))
        .route("/manifest.webmanifest", get(manifest))
        .route("/icon.png", get(icon))
        .route("/api/sessions", get(sessions))
        .route("/api/content", get(content))
        .route("/api/input", axum::routing::post(input))
        .layer(tower_http::compression::CompressionLayer::new())
        .with_state(state);
    // Default to all interfaces so a virtual LAN (e.g. 蒲公英/Tailscale) can
    // reach the page; set WARP_REMOTE_BIND=127.0.0.1:9283 to keep it local.
    let bind_addr = std::env::var("WARP_REMOTE_BIND").unwrap_or_else(|_| "0.0.0.0:9283".to_owned());
    let listener = match tokio::net::TcpListener::bind(&bind_addr).await {
        Ok(listener) => listener,
        Err(err) => {
            log::warn!("remote session bridge could not bind {bind_addr}: {err}");
            return;
        }
    };
    log::info!("remote session bridge listening on http://{bind_addr}");
    if let Err(err) = axum::serve(listener, router).await {
        log::warn!("remote session bridge stopped: {err}");
    }
}

async fn page() -> Html<&'static str> {
    Html(PAGE)
}

async fn manifest(Query(query): Query<ManifestQuery>) -> Json<Value> {
    // Echo the page token back into start_url so a home-screen launch keeps
    // authenticating; the manifest itself is public (it names the app, nothing else).
    let start_url = if query.token.is_empty() {
        "/".to_owned()
    } else {
        format!("/?token={}", query.token)
    };
    Json(json!({
        "name": "Warp 会话",
        "short_name": "Warp",
        "start_url": start_url,
        "display": "standalone",
        "background_color": "#101114",
        "theme_color": "#101114",
        "icons": [{ "src": "/icon.png", "sizes": "512x512", "type": "image/png" }],
    }))
}

async fn icon() -> impl axum::response::IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        axum::body::Bytes::from_static(ICON),
    )
}

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get("x-warp-token")
        .and_then(|value| value.to_str().ok())
        == Some(token)
}

async fn sessions(
    State(state): State<BridgeState>,
    headers: HeaderMap,
    Query(query): Query<SessionsQuery>,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let sessions = state
        .spawner
        .spawn(|_bridge, ctx| list_sessions_detailed(ctx))
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    // Stats ride along even when the session list is unchanged, so the
    // resource strip keeps updating at poll cadence.
    let stats = host_stats();
    let version = json_version(&sessions);
    if !query.v.is_empty() && version == query.v {
        return Ok(Json(json!({
            "ok": true,
            "changed": false,
            "stats": stats,
            "wechatBroken": wechat::send_broken(),
        })));
    }
    Ok(Json(json!({
        "ok": true,
        "sessions": sessions,
        "version": version,
        "readOnlyBridge": state.read_only,
        "stats": stats,
        "wechatBroken": wechat::send_broken(),
    })))
}

fn list_sessions(ctx: &mut ModelContext<RemoteSessionBridge>) -> Vec<Value> {
    let mut sessions = Vec::new();
    for window_id in ctx.window_ids() {
        let Some(workspaces) = ctx.views_of_type::<Workspace>(window_id) else {
            continue;
        };
        for workspace in workspaces {
            let workspace_ref = workspace.as_ref(ctx);
            for (index, tab) in workspace_ref.tabs.iter().enumerate() {
                let pane_group = tab.pane_group.as_ref(ctx);
                let title = pane_group.display_title(ctx);
                for view in pane_group.visible_terminal_views(ctx) {
                    sessions.push(json!({
                        "id": view.id().to_string(),
                        "title": title.clone(),
                        "tab": index + 1,
                        "readOnly": view.as_ref(ctx).is_read_only(),
                    }));
                }
            }
        }
    }
    sessions
}

/// Page-facing listing: adds the CLI agent state and a short tail of the most
/// recent output so the phone can show status badges and unread markers
/// without opening every session.
fn list_sessions_detailed(ctx: &mut ModelContext<RemoteSessionBridge>) -> Vec<Value> {
    // Two passes: traversing the pane graph borrows ctx immutably, while the
    // per-terminal content read needs &mut — so collect handles first and read
    // after the traversal ends.
    let mut views = Vec::new();
    let window_ids = ctx.window_ids().collect::<Vec<_>>();
    for window_id in window_ids {
        let Some(workspaces) = ctx.views_of_type::<Workspace>(window_id) else {
            continue;
        };
        for workspace in workspaces {
            let workspace_ref = workspace.as_ref(ctx);
            for (index, tab) in workspace_ref.tabs.iter().enumerate() {
                let pane_group = tab.pane_group.as_ref(ctx);
                let title = pane_group.display_title(ctx);
                for view in pane_group.visible_terminal_views(ctx) {
                    let read_only = view.as_ref(ctx).is_read_only();
                    views.push((view, title.clone(), index + 1, read_only));
                }
            }
        }
    }
    views
        .into_iter()
        .map(|(view, title, tab, read_only)| {
            let agent = CLIAgentSessionsModel::as_ref(ctx)
                .session(view.id())
                .map(|session| {
                    json!({
                        "name": session.agent.display_name(),
                        "status": wechat::status_tag(&session.status),
                    })
                });
            json!({
                "id": view.id().to_string(),
                "title": title,
                "tab": tab,
                "readOnly": read_only,
                "agent": agent,
                "tail": terminal_tail(&view, ctx),
            })
        })
        .collect()
}

fn terminal_tail(
    view: &warpui::ViewHandle<TerminalView>,
    ctx: &mut ModelContext<RemoteSessionBridge>,
) -> String {
    terminal_tail_view(view, ctx, PREVIEW_CHARS)
}

fn terminal_tail_view(
    view: &warpui::ViewHandle<TerminalView>,
    ctx: &mut ModelContext<RemoteSessionBridge>,
    max_chars: usize,
) -> String {
    let text = {
        let model = view.as_ref(ctx).model.lock();
        if model.is_alt_screen_active() {
            model.alt_screen().output_to_string_obfuscated()
        } else {
            let mut chunks = Vec::new();
            let mut total = 0;
            for block in model.block_list().blocks().iter().rev() {
                let block_text = block.command_and_output_to_string();
                if block_text.trim().is_empty() {
                    continue;
                }
                total += block_text.len();
                chunks.push(block_text);
                if total >= max_chars * 4 {
                    break;
                }
            }
            chunks.reverse();
            chunks.join("\n")
        }
    };
    tail_chars(&text, max_chars)
}

/// Recent output quoted in WeChat pushes for statuses that need a look before
/// acting (blocked / failed).
fn push_tail(ctx: &mut ModelContext<RemoteSessionBridge>, id: EntityId) -> String {
    let Some(view) = find_terminal(ctx, &id.to_string()) else {
        return String::new();
    };
    terminal_tail_view(&view, ctx, 500)
}

#[derive(Deserialize)]
struct ContentQuery {
    id: String,
    #[serde(default)]
    v: String,
}

#[derive(Deserialize)]
struct SessionsQuery {
    #[serde(default)]
    v: String,
}

#[derive(Deserialize)]
struct ManifestQuery {
    #[serde(default)]
    token: String,
}

async fn content(
    State(state): State<BridgeState>,
    headers: HeaderMap,
    Query(query): Query<ContentQuery>,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let snapshot = state
        .spawner
        .spawn(move |_bridge, ctx| read_content(ctx, &query.id))
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let Some(snapshot) = snapshot else {
        return Err(StatusCode::NOT_FOUND);
    };
    let version = snapshot["version"].as_str().unwrap_or_default().to_owned();
    if !query.v.is_empty() && version == query.v {
        return Ok(Json(json!({ "ok": true, "changed": false })));
    }
    Ok(Json(json!({ "ok": true, "content": snapshot })))
}

fn read_content(ctx: &mut ModelContext<RemoteSessionBridge>, id: &str) -> Option<Value> {
    let view = find_terminal(ctx, id)?;
    let text = {
        let model = view.as_ref(ctx).model.lock();
        if model.is_alt_screen_active() {
            model.alt_screen().output_to_string_obfuscated()
        } else {
            let mut chunks = Vec::new();
            let mut total = 0;
            for block in model.block_list().blocks().iter().rev() {
                let block_text = block.command_and_output_to_string();
                if block_text.trim().is_empty() {
                    continue;
                }
                total += block_text.len();
                chunks.push(block_text);
                // Byte budget with a ×4 headroom so CJK (3 bytes/char) still
                // fills the whole character window; tail_chars caps after.
                if total >= MAX_CONTENT_CHARS * 4 {
                    break;
                }
            }
            chunks.reverse();
            chunks.join("\n")
        }
    };
    let text = tail_chars(&text, MAX_CONTENT_CHARS);
    let version = json_version(&text);
    Some(json!({
        "text": text,
        "version": version,
        "readOnly": true,
    }))
}

/// Cheap change-detection token so idle polls can skip re-sending the body.
fn json_version(value: &impl serde::Serialize) -> String {
    let Ok(text) = serde_json::to_string(value) else {
        return String::new();
    };
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::hash::Hash::hash(&text.len(), &mut hasher);
    std::hash::Hash::hash(&text, &mut hasher);
    format!("{:x}", std::hash::Hasher::finish(&hasher))
}

#[derive(Deserialize)]
struct InputBody {
    id: String,
    text: String,
    #[serde(default)]
    enter: bool,
}

async fn input(
    State(state): State<BridgeState>,
    headers: HeaderMap,
    Json(body): Json<InputBody>,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    if state.read_only {
        return Err(StatusCode::FORBIDDEN);
    }
    let sent = state
        .spawner
        .spawn(move |_bridge, ctx| send_input(ctx, &body.id, &body.text, body.enter))
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    if !sent {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(json!({ "ok": true })))
}

fn send_input(
    ctx: &mut ModelContext<RemoteSessionBridge>,
    id: &str,
    text: &str,
    enter: bool,
) -> bool {
    let Some(view) = find_terminal(ctx, id) else {
        return false;
    };
    if view.as_ref(ctx).is_read_only() {
        return false;
    }
    let mut bytes = text.as_bytes().to_vec();
    if enter {
        bytes.push(b'\r');
    }
    view.update(ctx, |terminal, ctx| {
        terminal.write_to_pty(bytes, ctx);
    });
    true
}

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    text.chars().skip(count - max_chars).collect()
}

fn agent_status(ctx: &mut ModelContext<RemoteSessionBridge>, id: &str) -> String {
    let Some(view) = find_terminal(ctx, id) else {
        return "当前会话已关闭，/ls 重新选择".to_owned();
    };
    let Some(session) = CLIAgentSessionsModel::as_ref(ctx).session(view.id()) else {
        return "这个会话里没有运行中的 CLI agent".to_owned();
    };
    let status = wechat::describe_status(&session.status);
    format!("{} · {status}", session.agent.display_name())
}

fn find_terminal(
    ctx: &mut ModelContext<RemoteSessionBridge>,
    id: &str,
) -> Option<warpui::ViewHandle<TerminalView>> {
    for window_id in ctx.window_ids() {
        let Some(workspaces) = ctx.views_of_type::<Workspace>(window_id) else {
            continue;
        };
        for workspace in workspaces {
            for tab in &workspace.as_ref(ctx).tabs {
                for view in tab.pane_group.as_ref(ctx).visible_terminal_views(ctx) {
                    if view.id().to_string() == id {
                        return Some(view);
                    }
                }
            }
        }
    }
    None
}
