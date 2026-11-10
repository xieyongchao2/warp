#!/usr/bin/env python3
"""Backup notification channels: Bark (iPhone push) and Windows toast.

Config lives beside the WeChat credentials in ``notify-backup.json``
(written by ``script/notify-backup-setup.py``):

    {"barkUrl": "https://api.day.app/<your-key>"}
"""

from __future__ import annotations

import json
import os
import subprocess
from pathlib import Path
from urllib import request as urlrequest

MAX_BODY = 900

_TOAST_PS1 = r"""
[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType=WindowsRuntime] | Out-Null
$template = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02)
$text = $template.GetElementsByTagName("text")
[void]$text.Item(0).AppendChild($template.CreateTextNode("Warp"))
[void]$text.Item(1).AppendChild($template.CreateTextNode('__BODY__'))
[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier("Warp").Show([Windows.UI.Notifications.ToastNotification]::new($template))
"""


def backup_config_path() -> Path:
    override = os.environ.get("WARP_WECHAT_CONFIG_DIR")
    base = Path(override) if override else Path(os.environ["APPDATA"]) / "warp-dev"
    return base / "notify-backup.json"


def load_backup_config() -> dict:
    try:
        return json.loads(backup_config_path().read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return {}


def send_bark(text: str, title: str = "Warp 通知") -> None:
    url = str(load_backup_config().get("barkUrl") or "").strip()
    if not url:
        raise RuntimeError("bark 未配置")
    body = json.dumps(
        {"title": title, "body": text[:MAX_BODY], "group": "warp"}, ensure_ascii=False
    ).encode("utf-8")
    req = urlrequest.Request(
        url, data=body, headers={"Content-Type": "application/json; charset=utf-8"}
    )
    with urlrequest.urlopen(req, timeout=10) as response:
        if response.status != 200:
            raise RuntimeError(f"bark HTTP {response.status}")


def toast(text: str) -> None:
    safe = text[:200].replace("'", "''")
    script = _TOAST_PS1.replace("__BODY__", safe)
    subprocess.run(
        ["powershell", "-NoProfile", "-Command", script],
        timeout=10,
        creationflags=getattr(subprocess, "CREATE_NO_WINDOW", 0),
        check=False,
    )
