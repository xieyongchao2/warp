"""Minimal WeChat ClawBot iLink client.

Speaks the public bot endpoints used by the official WeChat ClawBot plugin:
QR login, long-poll receive, and sendmessage. No third-party runtime.
"""

from __future__ import annotations

import json
import secrets
import time
import urllib.error
import urllib.request
from urllib.parse import quote

FIXED_BASE_URL = "https://ilinkai.weixin.qq.com"
APP_ID = "bot"
CHANNEL_VERSION = "2.4.8"
CLIENT_VERSION = (2 << 16) | (4 << 8) | 8
BOT_AGENT = "WarpNotify/0.1.0"
API_TIMEOUT_SECONDS = 15
QR_TIMEOUT_SECONDS = 40
TEXT_CHUNK = 3500


class IlinkError(RuntimeError):
    pass


def normalize_base_url(base_url: str | None) -> str:
    raw = (base_url or FIXED_BASE_URL).strip()
    return raw if raw.endswith("/") else f"{raw}/"


def random_wechat_uin() -> str:
    import base64

    return base64.b64encode(str(secrets.randbits(32)).encode("ascii")).decode("ascii")


def base_info() -> dict:
    return {"channel_version": CHANNEL_VERSION, "bot_agent": BOT_AGENT}


def common_headers() -> dict[str, str]:
    return {
        "iLink-App-Id": APP_ID,
        "iLink-App-ClientVersion": str(CLIENT_VERSION),
    }


def auth_headers(token: str | None) -> dict[str, str]:
    headers = {
        "Content-Type": "application/json",
        "AuthorizationType": "ilink_bot_token",
        "X-WECHAT-UIN": random_wechat_uin(),
        **common_headers(),
    }
    if token:
        headers["Authorization"] = f"Bearer {token}"
    return headers


def request_json(
    url: str,
    *,
    method: str = "POST",
    body: dict | None = None,
    headers: dict[str, str] | None = None,
    timeout: float = API_TIMEOUT_SECONDS,
) -> dict:
    data = None if body is None else json.dumps(body, ensure_ascii=False).encode("utf-8")
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as response:
            raw = response.read()
    except urllib.error.HTTPError as err:
        detail = err.read().decode("utf-8", "replace")[:300]
        raise IlinkError(f"{method} {url} -> HTTP {err.code}: {detail}") from err
    except urllib.error.URLError as err:
        raise IlinkError(f"无法连接微信 ClawBot: {err.reason}") from err
    if not raw:
        return {}
    try:
        parsed = json.loads(raw.decode("utf-8"))
    except json.JSONDecodeError as err:
        raise IlinkError("微信 ClawBot 返回了无法解析的响应") from err
    if not isinstance(parsed, dict):
        raise IlinkError("微信 ClawBot 返回了意外的响应")
    return parsed


def check_ret(resp: dict, action: str) -> None:
    ret = resp.get("ret")
    if ret not in (None, 0):
        raise IlinkError(f"{action} ret={ret} errmsg={resp.get('errmsg') or '(none)'}")


def get_bot_qrcode(*, base_url: str = FIXED_BASE_URL, local_tokens: list[str] | None = None) -> dict:
    url = f"{normalize_base_url(base_url)}ilink/bot/get_bot_qrcode?bot_type=3"
    return request_json(
        url,
        headers=common_headers(),
        body={"local_token_list": local_tokens or []},
    )


def get_qr_status(*, base_url: str, qrcode: str, verify_code: str | None = None) -> dict:
    endpoint = f"ilink/bot/get_qrcode_status?qrcode={quote(qrcode)}"
    if verify_code:
        endpoint += f"&verify_code={quote(verify_code)}"
    try:
        return request_json(
            f"{normalize_base_url(base_url)}{endpoint}",
            method="GET",
            headers=common_headers(),
            timeout=QR_TIMEOUT_SECONDS,
        )
    except IlinkError as err:
        if "timed out" in str(err).lower():
            return {"status": "wait"}
        raise


def get_updates(*, base_url: str, token: str, cursor: str = "", timeout_ms: int = 35_000) -> dict:
    timeout = max(API_TIMEOUT_SECONDS, timeout_ms / 1000 + 5)
    return request_json(
        f"{normalize_base_url(base_url)}ilink/bot/getupdates",
        headers=auth_headers(token),
        body={"get_updates_buf": cursor or "", "base_info": base_info()},
        timeout=timeout,
    )


def chunk_text(text: str, limit: int = TEXT_CHUNK) -> list[str]:
    rest = text
    chunks: list[str] = []
    while len(rest) > limit:
        cut = rest.rfind("\n", 0, limit)
        if cut < limit // 2:
            cut = limit
        chunks.append(rest[:cut])
        rest = rest[cut:].lstrip("\n")
    if rest:
        chunks.append(rest)
    return chunks


def send_text(*, base_url: str, token: str, to: str, text: str, context_token: str) -> None:
    if not context_token:
        raise IlinkError("缺少微信会话上下文，请先在微信里给 ClawBot 发一条消息")
    url = f"{normalize_base_url(base_url)}ilink/bot/sendmessage"
    for chunk in chunk_text(text):
        client_id = f"warp:{int(time.time() * 1000)}-{secrets.token_hex(4)}"
        resp = request_json(
            url,
            headers=auth_headers(token),
            body={
                "msg": {
                    "from_user_id": "",
                    "to_user_id": to,
                    "client_id": client_id,
                    "message_type": 2,
                    "message_state": 2,
                    "item_list": [{"type": 1, "text_item": {"text": chunk}}],
                    "context_token": context_token,
                },
                "base_info": base_info(),
            },
        )
        check_ret(resp, "sendmessage")
