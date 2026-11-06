//! WeChat ClawBot command channel.
//!
//! Long-polls the iLink `getupdates` endpoint for messages from the bound
//! WeChat account and routes them as commands against open terminal sessions,
//! reusing the same PTY-write and content-read paths as the local page. Only
//! the account holder's `userId` is accepted; every other sender is dropped.
//! Credentials are shared with the Python notifier in
//! `%APPDATA%\warp-dev\wechat-clawbot.json`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedReceiver;
use warpui::ModelSpawner;

use super::RemoteSessionBridge;
use crate::terminal::cli_agent_sessions::{BlockedSource, CLIAgentSessionStatus};

const CHANNEL_VERSION: &str = "2.4.8";
const CLIENT_VERSION: u32 = (2 << 16) | (4 << 8) | 8;
const BOT_AGENT: &str = "WarpChannel/0.1.0";
const DEFAULT_BASE_URL: &str = "https://ilinkai.weixin.qq.com";
const POLL_TIMEOUT: Duration = Duration::from_secs(50);
const MIN_BACKOFF: Duration = Duration::from_secs(2);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
const MAX_MESSAGE_CHARS: usize = 3500;
const DEFAULT_TAIL_LINES: usize = 30;
const MAX_TAIL_LINES: usize = 200;
const MAX_GREP_LINES: usize = 40;
const DEDUPE_LIMIT: usize = 256;

pub(super) fn enabled() -> bool {
    std::env::var("WARP_WECHAT_CHANNEL")
        .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

#[derive(Clone, Default)]
struct ClawBotConfig {
    base_url: String,
    token: String,
    account_id: String,
    user_id: String,
    context_tokens: HashMap<String, String>,
    cursor: String,
    active_session: String,
}

impl ClawBotConfig {
    fn from_json(value: &Value) -> Self {
        let str_field = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let mut context_tokens = HashMap::new();
        if let Some(tokens) = value.get("contextTokens").and_then(Value::as_object) {
            for (user, token) in tokens {
                if let Some(token) = token.as_str() {
                    context_tokens.insert(user.clone(), token.to_owned());
                }
            }
        }
        Self {
            base_url: str_field("baseUrl"),
            token: str_field("token"),
            account_id: str_field("accountId"),
            user_id: str_field("userId"),
            context_tokens,
            cursor: str_field("cursor"),
            active_session: str_field("activeSession"),
        }
    }

    fn to_json(&self) -> Value {
        let mut tokens = serde_json::Map::new();
        for (user, token) in &self.context_tokens {
            tokens.insert(user.clone(), Value::String(token.clone()));
        }
        let base_url = if self.base_url.is_empty() {
            DEFAULT_BASE_URL
        } else {
            self.base_url.as_str()
        };
        json!({
            "baseUrl": base_url,
            "token": self.token,
            "accountId": self.account_id,
            "userId": self.user_id,
            "contextTokens": tokens,
            "cursor": self.cursor,
            "activeSession": self.active_session,
        })
    }
}

fn config_path() -> Option<PathBuf> {
    // Keep parity with the Python tools, which honor this override first.
    if let Ok(dir) = std::env::var("WARP_WECHAT_CONFIG_DIR")
        && !dir.is_empty()
    {
        return Some(PathBuf::from(dir).join("wechat-clawbot.json"));
    }
    std::env::var("APPDATA").ok().map(|appdata| {
        PathBuf::from(appdata)
            .join("warp-dev")
            .join("wechat-clawbot.json")
    })
}

fn load_config() -> Option<ClawBotConfig> {
    let path = config_path()?;
    let text = std::fs::read_to_string(path).ok()?;
    let value: Value = serde_json::from_str(&text).ok()?;
    Some(ClawBotConfig::from_json(&value))
}

fn save_config(config: &ClawBotConfig) {
    // The Python login tool refreshes token/contextTokens on the same file;
    // merge disk state so a stale in-memory snapshot cannot clobber a fresh
    // login. Cursor and activeSession stay owned by this channel.
    let mut merged = config.clone();
    if let Some(disk) = load_config() {
        if !disk.token.is_empty() {
            merged.token = disk.token;
        }
        if !disk.user_id.is_empty() {
            merged.user_id = disk.user_id;
        }
        if !disk.account_id.is_empty() {
            merged.account_id = disk.account_id;
        }
        if !disk.base_url.is_empty() {
            merged.base_url = disk.base_url;
        }
        for (user, token) in disk.context_tokens {
            merged.context_tokens.entry(user).or_insert(token);
        }
    }
    let Some(path) = config_path() else {
        return;
    };
    let Some(dir) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(dir).is_err() {
        log::warn!(
            "wechat channel could not create config dir {}",
            dir.display()
        );
        return;
    }
    let Ok(text) = serde_json::to_string_pretty(&merged.to_json()) else {
        return;
    };
    // persist() replaces the destination atomically on Windows, so a crash
    // between write and rename can never leave the credentials file deleted.
    let write = || {
        let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
        std::io::Write::write_all(&mut tmp, text.as_bytes())?;
        tmp.persist(&path).map_err(|err| err.error)?;
        Ok::<(), std::io::Error>(())
    };
    if write().is_err() {
        // A failed cursor save widens the restart-replay window, so retry
        // once before giving up (AV scans and editors briefly lock files).
        std::thread::sleep(Duration::from_millis(200));
        if let Err(retry) = write() {
            log::error!("wechat channel failed to save config: {retry}");
        }
    }
}

pub(super) async fn run(
    spawner: ModelSpawner<RemoteSessionBridge>,
    mut notifications: UnboundedReceiver<String>,
) {
    let Some(mut config) =
        load_config().filter(|config| !config.token.is_empty() && !config.user_id.is_empty())
    else {
        log::info!("wechat channel disabled: run script/wechat-clawbot-login.py first");
        return;
    };
    if config.base_url.is_empty() {
        config.base_url = DEFAULT_BASE_URL.to_owned();
    }
    let http = match reqwest::Client::builder().timeout(POLL_TIMEOUT).build() {
        Ok(http) => http,
        Err(err) => {
            log::warn!("wechat channel failed to build http client: {err}");
            return;
        }
    };
    log::info!(
        "wechat channel started for account {} (user {}); note that only one \
         getupdates consumer may run at a time, so stop script/wechat-clawbot-login.py --refresh \
         while this channel is active",
        config.account_id,
        config.user_id
    );
    let mut dedupe = MessageDedupe::default();
    let mut backoff = MIN_BACKOFF;
    loop {
        // Racing the long poll means an approval request reaches WeChat right
        // away instead of waiting out the poll timeout.
        tokio::select! {
            Some(text) = notifications.recv() => {
                if let Err(err) = send_reply(&http, &config, &text).await {
                    log::warn!("wechat channel failed to push agent status: {err}");
                }
            }
            result = get_updates(&http, &config) => match result {
                Ok(response) => {
                    backoff = MIN_BACKOFF;
                    for message in response.msgs {
                        handle_message(&http, &spawner, &mut config, &mut dedupe, message).await;
                    }
                    if let Some(cursor) = response.get_updates_buf {
                        config.cursor = cursor;
                        save_config(&config);
                    }
                    // A server or middlebox that answers immediately would turn
                    // this loop into an unbounded request rate.
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
                Err(err) => {
                    log::warn!("wechat channel poll failed: {err}");
                    // A fresh login written by the Python tool only helps if the
                    // live loop picks it up; otherwise an expired token wedges
                    // this channel until the app restarts.
                    if (err.contains("超时") || err.contains("-14"))
                        && let Some(fresh) = load_config()
                        && (fresh.token != config.token || fresh.user_id != config.user_id)
                    {
                        log::info!("wechat channel reloading refreshed credentials");
                        if !fresh.token.is_empty() {
                            config.token = fresh.token;
                        }
                        if !fresh.user_id.is_empty() {
                            config.user_id = fresh.user_id;
                        }
                        if !fresh.base_url.is_empty() {
                            config.base_url = fresh.base_url;
                        }
                        for (user, token) in fresh.context_tokens {
                            config.context_tokens.insert(user, token);
                        }
                        backoff = MIN_BACKOFF;
                    }
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            },
        }
    }
}

async fn handle_message(
    http: &reqwest::Client,
    spawner: &ModelSpawner<RemoteSessionBridge>,
    config: &mut ClawBotConfig,
    dedupe: &mut MessageDedupe,
    message: InboundMessage,
) {
    if message.from_user_id != config.user_id {
        log::info!(
            "wechat channel dropped message from unknown sender {}",
            message.from_user_id
        );
        return;
    }
    if let Some(token) = message.context_token.clone() {
        if config.context_tokens.get(&config.user_id) != Some(&token) {
            config.context_tokens.insert(config.user_id.clone(), token);
            save_config(config);
        }
    }
    let key = message.dedupe_key();
    if !dedupe.push(key) {
        return;
    }
    let text = message.text();
    if text.trim().is_empty() {
        return;
    }
    // The command already ran against the terminal, so silence here would
    // invite a duplicate send; always answer, and retry once on failure.
    let mut reply = dispatch(spawner, config, &text).await;
    if reply.trim().is_empty() {
        reply = "（无输出）".to_owned();
    }
    if let Err(err) = send_reply(http, config, &reply).await {
        log::warn!("wechat channel failed to reply: {err}");
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Err(retry_err) = send_reply(http, config, &reply).await {
            log::error!("wechat channel reply retry failed: {retry_err}");
        }
    }
}

async fn dispatch(
    spawner: &ModelSpawner<RemoteSessionBridge>,
    config: &mut ClawBotConfig,
    text: &str,
) -> String {
    match parse_command(text) {
        Command::Help => help_text(),
        Command::Ls => match spawner.spawn(|_, ctx| super::list_sessions(ctx)).await {
            Ok(sessions) => format_session_list(&sessions),
            Err(_) => "暂时无法读取会话".to_owned(),
        },
        Command::Use(selector) => {
            if selector.is_empty() {
                return "用法：/use <编号|名称>".to_owned();
            }
            match spawner
                .spawn(move |_, ctx| {
                    let sessions = super::list_sessions(ctx);
                    match_session(&sessions, &selector).map(|session| {
                        (
                            session["id"].as_str().unwrap_or_default().to_owned(),
                            session.clone(),
                        )
                    })
                })
                .await
            {
                Ok(Some((id, session))) => {
                    config.active_session = id;
                    save_config(config);
                    let title = session["title"].as_str().unwrap_or("会话");
                    format!("已切换到：{title}")
                }
                Ok(None) => "没找到这个会话，/ls 查看列表".to_owned(),
                Err(_) => "暂时无法读取会话".to_owned(),
            }
        }
        Command::Send(command) => {
            if command.is_empty() {
                return "用法：/send <指令>".to_owned();
            }
            send_to_active(spawner, config, command.as_bytes(), true).await
        }
        Command::Stop => send_to_active(spawner, config, &[0x03], false).await,
        Command::Tail(lines) => {
            if config.active_session.is_empty() {
                return "还没有选择会话，先 /use <编号>".to_owned();
            }
            let lines = lines.clamp(1, MAX_TAIL_LINES);
            let active_id = config.active_session.clone();
            match spawner
                .spawn(move |_, ctx| {
                    super::read_content(ctx, &active_id)
                        .map(|snapshot| snapshot["text"].as_str().unwrap_or_default().to_owned())
                })
                .await
            {
                Ok(Some(text)) => tail_lines(&text, lines),
                Ok(None) => "当前会话已关闭，/ls 重新选择".to_owned(),
                Err(_) => "暂时无法读取会话".to_owned(),
            }
        }
        Command::Status => {
            if config.active_session.is_empty() {
                return "还没有选择会话，先 /use <编号>".to_owned();
            }
            let active_id = config.active_session.clone();
            match spawner
                .spawn(move |_, ctx| super::agent_status(ctx, &active_id))
                .await
            {
                Ok(status) => status,
                Err(_) => "暂时无法读取状态".to_owned(),
            }
        }
        Command::Grep(pattern) => {
            if pattern.is_empty() {
                return "用法：/grep <关键词>".to_owned();
            }
            if config.active_session.is_empty() {
                return "还没有选择会话，先 /use <编号>".to_owned();
            }
            let active_id = config.active_session.clone();
            match spawner
                .spawn(move |_, ctx| {
                    super::read_content(ctx, &active_id)
                        .map(|snapshot| snapshot["text"].as_str().unwrap_or_default().to_owned())
                })
                .await
            {
                Ok(Some(text)) => grep_lines(&text, &pattern),
                Ok(None) => "当前会话已关闭，/ls 重新选择".to_owned(),
                Err(_) => "暂时无法读取会话".to_owned(),
            }
        }
        Command::Unknown(command) => {
            format!("未知指令 {command}\n\n{}", help_text())
        }
    }
}

async fn send_to_active(
    spawner: &ModelSpawner<RemoteSessionBridge>,
    config: &ClawBotConfig,
    bytes: &[u8],
    enter: bool,
) -> String {
    if config.active_session.is_empty() {
        return "还没有选择会话，先 /use <编号>".to_owned();
    }
    let id = config.active_session.clone();
    let payload = bytes.to_vec();
    match spawner
        .spawn(move |_, ctx| {
            let text = String::from_utf8_lossy(&payload).into_owned();
            super::send_input(ctx, &id, &text, enter)
        })
        .await
    {
        Ok(true) => "已发送".to_owned(),
        Ok(false) => "发送失败：会话已关闭或只读".to_owned(),
        Err(_) => "暂时无法发送".to_owned(),
    }
}

fn help_text() -> String {
    "\
Warp 微信指令
/ls  列出会话
/use <编号|名称>  选择会话
/send <指令>  发送到当前会话
/tail [行数]  查看最近输出
/grep <关键词>  在最近输出里查找
/status  查看当前会话的 agent 状态
/stop  发送 Ctrl+C
/help  帮助
直接发文本等于 /send"
        .to_owned()
}

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Ls,
    Use(String),
    Send(String),
    Stop,
    Tail(usize),
    Status,
    Grep(String),
    Unknown(String),
}

fn parse_command(text: &str) -> Command {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return Command::Send(trimmed.to_owned());
    }
    let mut parts = trimmed.splitn(2, ' ');
    let head = parts.next().unwrap_or_default();
    let rest = parts.next().unwrap_or_default().trim();
    match head {
        "/help" => Command::Help,
        "/ls" => Command::Ls,
        "/use" => Command::Use(rest.to_owned()),
        "/send" => Command::Send(rest.to_owned()),
        "/stop" => Command::Stop,
        "/status" => Command::Status,
        "/grep" => Command::Grep(rest.to_owned()),
        "/tail" => Command::Tail(
            rest.parse()
                .ok()
                .filter(|lines| *lines > 0)
                .unwrap_or(DEFAULT_TAIL_LINES),
        ),
        other => Command::Unknown(other.to_owned()),
    }
}

fn format_session_list(sessions: &[Value]) -> String {
    if sessions.is_empty() {
        return "当前没有打开的会话".to_owned();
    }
    let mut out = String::from("会话列表：");
    for (index, session) in sessions.iter().enumerate() {
        // Titles come from terminal escape sequences; newlines could forge
        // extra numbered entries in this listing.
        let title = session["title"]
            .as_str()
            .unwrap_or("未命名")
            .replace(['\n', '\r'], " ");
        let tab = session["tab"].as_u64().unwrap_or(0);
        let state = if session["readOnly"].as_bool().unwrap_or(false) {
            " · 只读"
        } else {
            ""
        };
        out.push_str(&format!("\n{}. {title}（标签 {tab}{state}）", index + 1));
    }
    out.push_str("\n/use <编号> 切换");
    out
}

fn match_session<'a>(sessions: &'a [Value], selector: &str) -> Option<&'a Value> {
    if let Ok(index) = selector.parse::<usize>() {
        if let Some(session) = sessions.get(index.wrapping_sub(1)) {
            return Some(session);
        }
        // Session ids are digit strings too; "/use 11" with two open sessions
        // means id 11, not an out-of-range index.
    }
    let selector = selector.to_lowercase();
    sessions
        .iter()
        .find(|session| {
            session["title"]
                .as_str()
                .is_some_and(|title| title.to_lowercase().contains(&selector))
        })
        .or_else(|| {
            sessions.iter().find(|session| {
                session["id"]
                    .as_str()
                    .is_some_and(|id| id.eq_ignore_ascii_case(&selector))
            })
        })
}

/// Stored per terminal so an identical status event does not push twice.
pub(super) fn status_tag(status: &CLIAgentSessionStatus) -> &'static str {
    match status {
        CLIAgentSessionStatus::InProgress => "in_progress",
        CLIAgentSessionStatus::Success => "success",
        CLIAgentSessionStatus::Failed { .. } => "failed",
        CLIAgentSessionStatus::Blocked { .. } => "blocked",
        CLIAgentSessionStatus::Cancelled => "cancelled",
    }
}

pub(super) fn describe_status(status: &CLIAgentSessionStatus) -> String {
    match status {
        CLIAgentSessionStatus::InProgress => "运行中".to_owned(),
        CLIAgentSessionStatus::Success => "已完成".to_owned(),
        CLIAgentSessionStatus::Failed {
            error_type,
            message,
        } => {
            let detail = message
                .as_deref()
                .or(error_type.as_deref())
                .unwrap_or("未提供原因");
            format!("失败：{detail}")
        }
        CLIAgentSessionStatus::Blocked { message, source } => {
            let reason = match source {
                BlockedSource::PermissionRequest => "等待授权",
                BlockedSource::QuestionAsked => "等待回答",
                BlockedSource::NeedsInput => "等待输入",
            };
            format!("{reason}：{}", message.as_deref().unwrap_or("（无说明）"))
        }
        CLIAgentSessionStatus::Cancelled => "已中断".to_owned(),
    }
}

/// The WeChat text pushed when a CLI agent changes status, or `None` for the
/// states that are not worth interrupting the user for.
pub(super) fn status_notification(
    agent: &str,
    title: String,
    status: &CLIAgentSessionStatus,
) -> Option<String> {
    let hint = match status {
        CLIAgentSessionStatus::Blocked { .. } => "直接回复 1 / 2 或 /send <内容>，/tail 查看上下文",
        CLIAgentSessionStatus::Success => "/tail 查看输出",
        CLIAgentSessionStatus::Failed { .. } => "/tail 查看完整输出",
        CLIAgentSessionStatus::InProgress | CLIAgentSessionStatus::Cancelled => return None,
    };
    let label = if title.is_empty() {
        agent.to_owned()
    } else {
        format!("{agent} · {title}")
    };
    Some(format!("【{label}】{}\n{hint}", describe_status(status)))
}

fn grep_lines(text: &str, pattern: &str) -> String {
    let needle = pattern.to_lowercase();
    let matches: Vec<&str> = text
        .lines()
        .filter(|line| line.to_lowercase().contains(&needle))
        .collect();
    if matches.is_empty() {
        return format!("没有匹配「{pattern}」的行");
    }
    let total = matches.len();
    let shown = total.saturating_sub(MAX_GREP_LINES);
    let header = if shown > 0 {
        format!("匹配 {total} 行，显示最后 {MAX_GREP_LINES} 行：")
    } else {
        format!("匹配 {total} 行：")
    };
    let body = super::tail_chars(&matches[shown..].join("\n"), MAX_MESSAGE_CHARS);
    format!("{header}\n{body}")
}

fn tail_lines(text: &str, lines: usize) -> String {
    let mut out: Vec<&str> = text.lines().collect();
    if out.len() > lines {
        out = out.split_off(out.len() - lines);
    }
    super::tail_chars(&out.join("\n"), MAX_MESSAGE_CHARS)
}

fn chunk_text(text: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = text;
    while rest.chars().count() > MAX_MESSAGE_CHARS {
        let limit = char_boundary(rest, MAX_MESSAGE_CHARS);
        // A newline in the first half of the window would produce an empty
        // chunk; fall back to a hard cut instead.
        let mut split = rest[..limit].rfind('\n').unwrap_or(limit);
        if split < limit / 2 {
            split = limit;
        }
        chunks.push(&rest[..split]);
        rest = rest[split..].strip_prefix('\n').unwrap_or(&rest[split..]);
    }
    if !rest.is_empty() {
        chunks.push(rest);
    }
    chunks
}

fn char_boundary(text: &str, max_chars: usize) -> usize {
    text.char_indices()
        .nth(max_chars)
        .map(|(offset, _)| offset)
        .unwrap_or(text.len())
}

#[derive(Default)]
struct MessageDedupe {
    seen: HashSet<String>,
    order: VecDeque<String>,
}

impl MessageDedupe {
    fn push(&mut self, key: String) -> bool {
        if self.seen.contains(&key) {
            return false;
        }
        self.seen.insert(key.clone());
        self.order.push_back(key);
        if self.order.len() > DEDUPE_LIMIT {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
        true
    }
}

#[derive(Debug, Default, Deserialize)]
struct UpdatesResponse {
    #[serde(default)]
    ret: Option<i64>,
    #[serde(default)]
    errcode: Option<i64>,
    #[serde(default)]
    errmsg: Option<String>,
    #[serde(default)]
    get_updates_buf: Option<String>,
    #[serde(default)]
    msgs: Vec<InboundMessage>,
}

#[derive(Debug, Default, Deserialize)]
struct InboundMessage {
    #[serde(default)]
    from_user_id: String,
    #[serde(default)]
    message_id: Option<Value>,
    #[serde(default)]
    create_time_ms: Option<f64>,
    #[serde(default)]
    context_token: Option<String>,
    #[serde(default)]
    item_list: Vec<InboundItem>,
}

impl InboundMessage {
    fn dedupe_key(&self) -> String {
        if let Some(id) = &self.message_id {
            return id.to_string();
        }
        // Server-less deliveries still need unique keys per command, or two
        // messages in the same millisecond (or any two without timestamps)
        // would collapse and the second one would be silently dropped.
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        std::hash::Hash::hash(&self.create_time_ms.map(|time| time.to_bits()), &mut hasher);
        std::hash::Hash::hash(&self.text(), &mut hasher);
        format!(
            "{}:{:x}",
            self.from_user_id,
            std::hash::Hasher::finish(&hasher)
        )
    }

    fn text(&self) -> String {
        self.item_list
            .iter()
            .filter_map(|item| item.text())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Default, Deserialize)]
struct InboundItem {
    #[serde(default, rename = "type")]
    kind: i64,
    #[serde(default)]
    text_item: Option<InboundTextItem>,
}

impl InboundItem {
    fn text(&self) -> Option<&str> {
        if self.kind != 1 {
            return None;
        }
        self.text_item
            .as_ref()
            .and_then(|item| item.text.as_deref())
            .filter(|text| !text.is_empty())
    }
}

#[derive(Debug, Default, Deserialize)]
struct InboundTextItem {
    #[serde(default)]
    text: Option<String>,
}

async fn get_updates(
    http: &reqwest::Client,
    config: &ClawBotConfig,
) -> Result<UpdatesResponse, String> {
    let url = format!(
        "{}/ilink/bot/getupdates",
        config.base_url.trim_end_matches('/')
    );
    let body = json!({
        "get_updates_buf": config.cursor,
        "base_info": { "channel_version": CHANNEL_VERSION, "bot_agent": BOT_AGENT },
    });
    let response = post_json(http, &url, &config.token, &body).await?;
    if response.errcode == Some(-14) {
        return Err("会话已超时，请在微信里给 ClawBot 发一条消息".to_owned());
    }
    if response.ret.is_some_and(|ret| ret != 0) {
        return Err(format!(
            "getupdates ret={} errmsg={}",
            response.ret.unwrap_or_default(),
            response.errmsg.unwrap_or_default()
        ));
    }
    Ok(response)
}

async fn send_reply(
    http: &reqwest::Client,
    config: &ClawBotConfig,
    text: &str,
) -> Result<(), String> {
    let Some(context_token) = config.context_tokens.get(&config.user_id) else {
        return Err("缺少 context_token，请在微信里给 ClawBot 发一条消息".to_owned());
    };
    let to = config.user_id.clone();
    let context_token = context_token.clone();
    for chunk in chunk_text(text) {
        let url = format!(
            "{}/ilink/bot/sendmessage",
            config.base_url.trim_end_matches('/')
        );
        let client_id = format!(
            "warpch:{}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default(),
            rand::random::<u32>()
        );
        let body = json!({
            "msg": {
                "from_user_id": "",
                "to_user_id": to,
                "client_id": client_id,
                "message_type": 2,
                "message_state": 2,
                "item_list": [{ "type": 1, "text_item": { "text": chunk } }],
                "context_token": context_token,
            },
            "base_info": { "channel_version": CHANNEL_VERSION, "bot_agent": BOT_AGENT },
        });
        let response = post_json(http, &url, &config.token, &body).await?;
        if response.ret.is_some_and(|ret| ret != 0) {
            return Err(format!(
                "sendmessage ret={} errmsg={}",
                response.ret.unwrap_or_default(),
                response.errmsg.unwrap_or_default()
            ));
        }
    }
    Ok(())
}

async fn post_json(
    http: &reqwest::Client,
    url: &str,
    token: &str,
    body: &Value,
) -> Result<UpdatesResponse, String> {
    let payload = serde_json::to_string(body).map_err(|err| err.to_string())?;
    let response = http
        .post(url)
        .header("Content-Type", "application/json")
        .header("Authorization", format!("Bearer {token}"))
        .header("AuthorizationType", "ilink_bot_token")
        .header("X-WECHAT-UIN", random_wechat_uin())
        .header("iLink-App-Id", "bot")
        .header("iLink-App-ClientVersion", CLIENT_VERSION.to_string())
        .body(payload)
        .send()
        .await
        .map_err(|err| err.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|err| err.to_string())?;
    if !status.is_success() {
        return Err(format!("HTTP {status}: {}", truncate(&text, 200)));
    }
    // An idle long-poll can come back as 200 with no body; that is a no-op,
    // not a parse failure.
    if text.trim().is_empty() {
        return Ok(UpdatesResponse::default());
    }
    serde_json::from_str::<UpdatesResponse>(&text).map_err(|err| format!("解析响应失败: {err}"))
}

fn random_wechat_uin() -> String {
    let number = rand::random::<u32>().to_string();
    base64::engine::general_purpose::STANDARD.encode(number)
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

#[cfg(test)]
#[path = "wechat_tests.rs"]
mod tests;
