//! Local page for controlling terminal sessions already open in this Warp process.
//!
//! The page binds to loopback only. Keystrokes go through `TerminalView::write_to_pty`,
//! which is the same path the UI uses, so this bridge never locks `TerminalModel`.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Html;
use axum::routing::get;
use serde::Deserialize;
use serde_json::{Value, json};
use warpui::{Entity, ModelContext, ModelSpawner, SingletonEntity};

use crate::terminal::view::TerminalView;
use crate::workspace::Workspace;

const PAGE: &str = include_str!("remote_session_page.html");
const MAX_CONTENT_CHARS: usize = 24_000;

#[derive(Clone)]
struct BridgeState {
    spawner: ModelSpawner<RemoteSessionBridge>,
    token: Arc<str>,
}

pub struct RemoteSessionBridge;

impl Entity for RemoteSessionBridge {
    type Event = ();
}

impl SingletonEntity for RemoteSessionBridge {}

impl RemoteSessionBridge {
    pub fn new(ctx: &mut ModelContext<Self>) -> Self {
        let token = std::env::var("WARP_REMOTE_TOKEN")
            .ok()
            .filter(|token| !token.is_empty())
            .unwrap_or_else(|| "warp-dev".to_owned());
        let state = BridgeState {
            spawner: ctx.spawner(),
            token: Arc::from(token),
        };
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_io()
            .build();
        match runtime {
            Ok(runtime) => {
                runtime.spawn(serve(state));
                std::mem::forget(runtime);
            }
            Err(err) => log::warn!("remote session bridge failed to start: {err}"),
        }
        Self
    }
}

async fn serve(state: BridgeState) {
    let router = axum::Router::new()
        .route("/", get(page))
        .route("/api/sessions", get(sessions))
        .route("/api/content", get(content))
        .with_state(state);
    let listener = match tokio::net::TcpListener::bind("127.0.0.1:9283").await {
        Ok(listener) => listener,
        Err(err) => {
            log::warn!("remote session bridge could not bind 127.0.0.1:9283: {err}");
            return;
        }
    };
    log::info!("remote session bridge listening on http://127.0.0.1:9283");
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

fn read_content(
    ctx: &mut ModelContext<RemoteSessionBridge>,
    id: &str,
) -> Option<Value> {
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

fn tail_chars(text: &str, max_chars: usize) -> String {
    let count = text.chars().count();
    if count <= max_chars {
        return text.to_owned();
    }
    text.chars().skip(count - max_chars).collect()
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
