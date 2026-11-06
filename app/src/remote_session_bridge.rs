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
const MAX_CONTENT_CHARS: usize = 24_000;

#[derive(Clone)]
struct BridgeState {
    spawner: ModelSpawner<RemoteSessionBridge>,
    token: Arc<str>,
}

pub struct RemoteSessionBridge {
    /// Present only when the WeChat channel is enabled; agent status changes are
    /// queued here and delivered by the channel's own long-poll task.
    notifier: Option<UnboundedSender<String>>,
    /// Last pushed status tag per terminal, so a repeated status does not re-notify.
    last_pushed: HashMap<EntityId, &'static str>,
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
                }
            }
            Err(err) => {
                log::warn!("remote session bridge failed to start: {err}");
                Self {
                    notifier: None,
                    last_pushed: HashMap::new(),
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
                let tag = wechat::status_tag(status);
                if self.last_pushed.get(terminal_view_id) == Some(&tag) {
                    return;
                }
                let title = session_title(ctx, *terminal_view_id);
                let Some(text) = wechat::status_notification(agent.display_name(), title, status)
                else {
                    return;
                };
                if notifier.send(text).is_err() {
                    log::debug!("wechat channel is not running; dropped an agent status push");
                    return;
                }
                self.last_pushed.insert(*terminal_view_id, tag);
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
    let router = axum::Router::new()
        .route("/", get(page))
        .route("/api/sessions", get(sessions))
        .route("/api/content", get(content))
        .route("/api/input", axum::routing::post(input))
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

fn authorized(headers: &HeaderMap, token: &str) -> bool {
    headers
        .get("x-warp-token")
        .and_then(|value| value.to_str().ok())
        == Some(token)
}

async fn sessions(
    State(state): State<BridgeState>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    if !authorized(&headers, &state.token) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    let sessions = state
        .spawner
        .spawn(|_bridge, ctx| list_sessions(ctx))
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    Ok(Json(json!({ "ok": true, "sessions": sessions })))
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

#[derive(Deserialize)]
struct ContentQuery {
    id: String,
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
    match snapshot {
        Some(snapshot) => Ok(Json(json!({ "ok": true, "content": snapshot }))),
        None => Err(StatusCode::NOT_FOUND),
    }
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
                if total >= MAX_CONTENT_CHARS {
                    break;
                }
            }
            chunks.reverse();
            chunks.join("\n")
        }
    };
    let text = tail_chars(&text, MAX_CONTENT_CHARS);
    Some(json!({
        "text": text,
        "readOnly": true,
    }))
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
