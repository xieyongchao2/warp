#!/usr/bin/env python3
"""Claude Code Stop hook: send a WeChat ClawBot notification directly.

Credentials live in %APPDATA%\\warp-dev\\wechat-clawbot.json. Run
``script/wechat-clawbot-login.py`` once to bind WeChat. This hook never
depends on another local app.
"""

from __future__ import annotations

import json
import os
import re
import sys
from datetime import datetime
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from wechat_ilink import IlinkError, send_text
from wechat_notify_config import config_path, context_token_for, load_config

import notify_backup

MAX_SUMMARY = 120


def clean_markdown(text: str) -> str:
    text = re.sub(r"```[\s\S]*?```", " ", text)
    text = re.sub(r"`([^`]*)`", r"\1", text)
    text = re.sub(r"!\[[^\]]*\]\([^)]*\)", " ", text)
    text = re.sub(r"\[([^\]]+)\]\([^)]*\)", r"\1", text)
    lines = []
    for line in text.splitlines():
        stripped = line.strip()
        if not stripped or stripped.startswith("|"):
            continue
        if re.fullmatch(r"[-—•*_=\s]{2,}", stripped):
            continue
        stripped = re.sub(r"^#{1,6}\s*", "", stripped)
        stripped = re.sub(r"^(?:[-*+>]\s+|\d+[.)]\s+)", "", stripped)
        stripped = stripped.replace("**", "").replace("__", "")
        lines.append(stripped)
    return " ".join(lines)


def summarize(payload: dict) -> str:
    text = payload.get("last_assistant_message")
    if not isinstance(text, str):
        return ""
    compact = clean_markdown(text)
    if len(compact) <= MAX_SUMMARY:
        return compact
    window = compact[:MAX_SUMMARY]
    cut = max(window.rfind(mark) for mark in "。！？.!?")
    if cut >= 40:
        return window[: cut + 1]
    return window.rstrip(" ，,；;") + "…"


def transcript_title(payload: dict) -> str:
    path = payload.get("transcript_path")
    if not isinstance(path, str) or not path:
        return ""
    session_id = payload.get("session_id")
    custom_title = ""
    ai_title = ""
    try:
        with open(path, encoding="utf-8", errors="replace") as transcript:
            for line in transcript:
                if '"custom-title"' not in line and '"ai-title"' not in line:
                    continue
                try:
                    record = json.loads(line)
                except json.JSONDecodeError:
                    continue
                record_session = record.get("sessionId")
                if session_id and record_session and record_session != session_id:
                    continue
                if record.get("type") == "custom-title":
                    custom_title = str(record.get("customTitle") or "").strip()
                elif record.get("type") == "ai-title":
                    ai_title = str(record.get("aiTitle") or "").strip()
    except OSError:
        return ""
    return custom_title or ai_title


def session_name(payload: dict) -> str:
    title = transcript_title(payload)
    if title:
        return title
    cwd = str(payload.get("cwd") or "")
    return os.path.basename(cwd.rstrip("\\/")) or "未知会话"


def build_message(payload: dict) -> str:
    lines = [
        "✅ Claude Code 已完成",
        f"💬 {session_name(payload)}   ·   🕒 {datetime.now():%H:%M}",
    ]
    summary = summarize(payload)
    lines.extend(["", summary or "会话已结束，打开 Warp 查看完整结果。"])
    return "\n".join(lines)


def read_payload() -> dict:
    # Claude Code writes UTF-8 JSON. Reading via the locale-encoded text stream
    # corrupts that payload on a Chinese Windows console.
    raw = sys.stdin.buffer.read()
    if not raw.strip():
        return {}
    try:
        payload = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, json.JSONDecodeError):
        return {}
    return payload if isinstance(payload, dict) else {}


def notify(payload: dict) -> None:
    config = load_config()
    token = config.get("token")
    user_id = config.get("userId")
    if not token or not user_id:
        raise IlinkError(f"尚未绑定微信，请先运行 script/wechat-clawbot-login.py（配置: {config_path()}）")
    context_token = context_token_for(config, user_id)
    if not context_token:
        raise IlinkError("缺少微信会话上下文，请先在微信里给 ClawBot 发一条消息")
    send_text(
        base_url=config.get("baseUrl") or "",
        token=token,
        to=user_id,
        text=build_message(payload),
        context_token=context_token,
    )


def append_log(message: str) -> None:
    try:
        path = config_path().with_name("notify-hook.log")
        path.parent.mkdir(parents=True, exist_ok=True)
        with path.open("a", encoding="utf-8") as log:
            log.write(f"{datetime.now():%Y-%m-%d %H:%M:%S} {message}\n")
    except OSError:
        pass


def main() -> int:
    payload = read_payload()
    append_log(f"invoked session={payload.get('session_id')} active={payload.get('stop_hook_active')}")
    # Claude sets this when a Stop hook already triggered another stop.
    if payload.get("stop_hook_active") is True:
        return 0
    message = build_message(payload)
    try:
        notify(payload)
        append_log("sent via wechat")
        return 0
    except Exception as err:
        append_log(f"wechat failed: {err}")
    # WeChat dies silently for hours once its context token expires; deliver
    # through the backup chain instead of dropping the notification.
    try:
        notify_backup.send_bark(message)
        append_log("sent via bark")
        return 0
    except Exception as err:
        append_log(f"bark failed: {err}")
    try:
        notify_backup.toast(message)
        append_log("sent via toast")
    except Exception as err:
        append_log(f"toast failed: {err}")
        sys.stderr.buffer.write(f"notify: all channels failed\n".encode("utf-8", "replace"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
