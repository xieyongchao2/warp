"""Local ClawBot credentials for the Warp WeChat notifier.

The file lives under %APPDATA%\\warp-dev and is never committed. On first use,
credentials already saved by 棱镜 are copied once so an existing WeChat binding
keeps working without that app staying open.
"""

from __future__ import annotations

import json
import os
from pathlib import Path

from wechat_ilink import FIXED_BASE_URL


def config_dir() -> Path:
    override = os.environ.get("WARP_WECHAT_CONFIG_DIR")
    if override:
        return Path(override)
    appdata = os.environ.get("APPDATA")
    if appdata:
        return Path(appdata) / "warp-dev"
    return Path.home() / "AppData" / "Roaming" / "warp-dev"


def config_path() -> Path:
    return config_dir() / "wechat-clawbot.json"


def legacy_config_path() -> Path | None:
    appdata = os.environ.get("APPDATA")
    if not appdata:
        return None
    return Path(appdata) / "warp-remote" / "config.json"


def empty_config() -> dict:
    return {
        "baseUrl": FIXED_BASE_URL,
        "token": "",
        "accountId": "",
        "userId": "",
        "contextTokens": {},
        "cursor": "",
    }


def read_json(path: Path) -> dict | None:
    try:
        data = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return data if isinstance(data, dict) else None


def write_config(config: dict) -> None:
    path = config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(f".{os.getpid()}.tmp")
    tmp.write_text(json.dumps(config, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    tmp.replace(path)


def from_legacy(raw: dict) -> dict | None:
    claw = raw.get("clawbot")
    if not isinstance(claw, dict) or not claw.get("token"):
        return None
    tokens = claw.get("contextTokens")
    return {
        "baseUrl": claw.get("baseUrl") or FIXED_BASE_URL,
        "token": claw.get("token") or "",
        "accountId": claw.get("accountId") or "",
        "userId": claw.get("userId") or "",
        "contextTokens": tokens if isinstance(tokens, dict) else {},
        "cursor": claw.get("cursor") or "",
    }


def load_config() -> dict:
    current = read_json(config_path())
    if current and current.get("token"):
        config = empty_config()
        config.update(current)
        if not isinstance(config.get("contextTokens"), dict):
            config["contextTokens"] = {}
        return config

    legacy_path = legacy_config_path()
    if legacy_path:
        imported = from_legacy(read_json(legacy_path) or {})
        if imported:
            write_config(imported)
            return imported
    return empty_config()


def context_token_for(config: dict, user_id: str) -> str:
    tokens = config.get("contextTokens")
    if not isinstance(tokens, dict):
        return ""
    token = tokens.get(user_id)
    return token if isinstance(token, str) else ""


def remember_context_token(config: dict, user_id: str, context_token: str) -> bool:
    if not user_id or not context_token:
        return False
    tokens = config.setdefault("contextTokens", {})
    if not isinstance(tokens, dict):
        tokens = {}
        config["contextTokens"] = tokens
    if tokens.get(user_id) == context_token:
        return False
    tokens[user_id] = context_token
    return True
