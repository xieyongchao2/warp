#!/usr/bin/env python3
"""Bind this machine's WeChat ClawBot account for Warp completion notices.

Prints a QR URL. Scan it in WeChat, confirm on the phone, then send ClawBot
one message so the completion hook has a context token.
"""

from __future__ import annotations

import argparse
import sys
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

from wechat_ilink import (
    FIXED_BASE_URL,
    IlinkError,
    get_bot_qrcode,
    get_qr_status,
    get_updates,
)
from wechat_notify_config import (
    config_path,
    load_config,
    remember_context_token,
    write_config,
)

LOGIN_TTL_SECONDS = 5 * 60
MAX_QR_REFRESH = 3


def write_out(text: str) -> None:
    sys.stdout.buffer.write(f"{text}\n".encode("utf-8", "replace"))
    sys.stdout.buffer.flush()


def fetch_qr(local_tokens: list[str]) -> tuple[str, str]:
    resp = get_bot_qrcode(local_tokens=local_tokens)
    qrcode = resp.get("qrcode")
    qr_url = resp.get("qrcode_img_content")
    if not qrcode or not qr_url:
        raise IlinkError("服务端未返回二维码")
    return str(qrcode), str(qr_url)


def save_confirmed(resp: dict) -> dict:
    account_id = resp.get("ilink_bot_id")
    if not account_id:
        raise IlinkError("登录失败：服务端未返回账号 ID")
    config = load_config()
    config.update(
        {
            "baseUrl": resp.get("baseurl") or FIXED_BASE_URL,
            "token": resp.get("bot_token") or "",
            "accountId": account_id,
            "userId": resp.get("ilink_user_id") or "",
            "contextTokens": {},
            "cursor": "",
        }
    )
    write_config(config)
    return config


def capture_context(config: dict) -> bool:
    user_id = config.get("userId") or ""
    resp = get_updates(
        base_url=config.get("baseUrl") or FIXED_BASE_URL,
        token=config["token"],
        cursor=config.get("cursor") or "",
        timeout_ms=20_000,
    )
    if resp.get("errcode") == -14:
        return False
    if resp.get("ret") not in (None, 0):
        raise IlinkError(f"getupdates ret={resp.get('ret')} errmsg={resp.get('errmsg') or '(none)'}")
    if resp.get("get_updates_buf"):
        config["cursor"] = resp["get_updates_buf"]
    changed = False
    for msg in resp.get("msgs") or []:
        if not isinstance(msg, dict):
            continue
        sender = msg.get("from_user_id")
        token = msg.get("context_token")
        if sender == user_id and isinstance(token, str):
            changed = remember_context_token(config, user_id, token) or changed
    if changed or resp.get("get_updates_buf"):
        write_config(config)
    return bool(config.get("contextTokens", {}).get(user_id))


def login() -> int:
    existing = load_config()
    local_tokens = [existing["token"]] if existing.get("token") else []
    base_url = FIXED_BASE_URL
    refresh_count = 0
    qrcode, qr_url = fetch_qr(local_tokens)
    write_out("请用微信扫描这个二维码：")
    write_out(qr_url)
    deadline = time.time() + LOGIN_TTL_SECONDS
    verify_code = ""

    while time.time() < deadline:
        resp = get_qr_status(base_url=base_url, qrcode=qrcode, verify_code=verify_code or None)
        status = resp.get("status")
        if status == "confirmed":
            config = save_confirmed(resp)
            write_out(f"已绑定。配置写在 {config_path()}")
            write_out("现在请在微信里给 ClawBot 发一条消息，用来获取通知所需的会话上下文。")
            context_deadline = time.time() + 90
            while time.time() < context_deadline:
                if capture_context(config):
                    write_out("会话上下文已保存，之后 Claude Code 完成时会直接发微信。")
                    return 0
                time.sleep(1)
            write_out("绑定已保存，但还没有会话上下文。稍后在微信里给 ClawBot 发一条消息即可。")
            return 0
        if status == "scaned":
            verify_code = ""
            write_out("已扫码，请在手机上确认。")
        elif status == "need_verifycode":
            write_out("请输入手机微信显示的数字：")
            verify_code = input().strip()
        elif status == "scaned_but_redirect" and resp.get("redirect_host"):
            base_url = f"https://{resp['redirect_host']}"
            write_out(f"已扫码，正在切换线路：{base_url}")
        elif status == "expired":
            refresh_count += 1
            if refresh_count > MAX_QR_REFRESH:
                raise IlinkError("二维码多次失效，请重新运行登录")
            write_out(f"二维码已过期，正在刷新 ({refresh_count}/{MAX_QR_REFRESH})")
            qrcode, qr_url = fetch_qr([])
            write_out(qr_url)
        elif status == "binded_redirect":
            raise IlinkError("该微信已绑定到其他宿主，请先在原宿主解绑后再扫码")
        elif status == "verify_code_blocked":
            raise IlinkError("数字多次输入错误，请稍后再试")
        time.sleep(1)
    raise IlinkError("登录超时，请重新运行")


def refresh() -> int:
    config = load_config()
    if not config.get("token") or not config.get("userId"):
        raise IlinkError("尚未绑定微信，请先不带参数运行本脚本扫码")
    write_out("请在微信里给 ClawBot 发一条消息，脚本会等待最多 90 秒。")
    deadline = time.time() + 90
    while time.time() < deadline:
        if capture_context(config):
            write_out(f"会话上下文已更新：{config_path()}")
            return 0
        time.sleep(1)
    raise IlinkError("没有收到新的微信消息，会话上下文未更新")


def main() -> int:
    parser = argparse.ArgumentParser(description="Bind or refresh WeChat ClawBot for Warp notices.")
    parser.add_argument("--refresh", action="store_true", help="Wait for one WeChat message and save its context token")
    args = parser.parse_args()
    try:
        return refresh() if args.refresh else login()
    except (IlinkError, EOFError, KeyboardInterrupt) as err:
        sys.stderr.buffer.write(f"{err}\n".encode("utf-8", "replace"))
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
