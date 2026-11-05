#!/usr/bin/env python3
"""Tests for the direct WeChat ClawBot notifier."""

from __future__ import annotations

import json
import os
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent
SCRIPT = ROOT / "claude-stop-wechat-notify.py"
PYTHON = sys.executable
sys.path.insert(0, str(ROOT))

import wechat_ilink
import wechat_notify_config
from wechat_ilink import IlinkError, send_text


class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length)
        self.server.requests.append(
            {
                "path": self.path,
                "authorization": self.headers.get("Authorization"),
                "body": json.loads(body.decode("utf-8")),
            }
        )
        payload = getattr(self.server, "payload", {"ret": 0})
        raw = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, fmt, *args):
        return


def test_send_text_uses_ilink_shape() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        send_text(
            base_url=f"http://127.0.0.1:{server.server_address[1]}",
            token="bot-token",
            to="user-1",
            text="hello",
            context_token="ctx-1",
        )
        req = server.requests[0]
        assert req["path"] == "/ilink/bot/sendmessage"
        assert req["authorization"] == "Bearer bot-token"
        msg = req["body"]["msg"]
        assert msg["to_user_id"] == "user-1"
        assert msg["context_token"] == "ctx-1"
        assert msg["item_list"] == [{"type": 1, "text_item": {"text": "hello"}}]
        assert req["body"]["base_info"]["bot_agent"] == "WarpNotify/0.1.0"
    finally:
        server.shutdown()


def test_send_text_reports_ilink_error() -> None:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.requests = []
    server.payload = {"ret": -2, "errmsg": "prepare failed"}
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        try:
            send_text(
                base_url=f"http://127.0.0.1:{server.server_address[1]}",
                token="bot-token",
                to="user-1",
                text="hello",
                context_token="ctx-1",
            )
        except IlinkError as err:
            assert "ret=-2" in str(err)
        else:
            raise AssertionError("expected IlinkError")
    finally:
        server.shutdown()


def test_imports_legacy_config_once(tmp: Path, monkey_env: dict[str, str]) -> None:
    legacy = tmp / "legacy" / "config.json"
    legacy.parent.mkdir()
    legacy.write_text(
        json.dumps(
            {
                "clawbot": {
                    "baseUrl": "https://ilink.example",
                    "token": "legacy-token",
                    "accountId": "bot-1",
                    "userId": "user-1",
                    "contextTokens": {"user-1": "ctx-1"},
                    "cursor": "cur",
                }
            }
        ),
        encoding="utf-8",
    )
    monkey_env["APPDATA"] = str(tmp / "roaming")
    original = wechat_notify_config.legacy_config_path
    wechat_notify_config.legacy_config_path = lambda: legacy
    try:
        config = wechat_notify_config.load_config()
        assert config["token"] == "legacy-token"
        assert config["contextTokens"]["user-1"] == "ctx-1"
        assert wechat_notify_config.config_path().exists()
    finally:
        wechat_notify_config.legacy_config_path = original


def test_hook_posts_completion(tmp: Path) -> None:
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.requests = []
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        config_dir = tmp / "config"
        config_dir.mkdir()
        (config_dir / "wechat-clawbot.json").write_text(
            json.dumps(
                {
                    "baseUrl": f"http://127.0.0.1:{server.server_address[1]}",
                    "token": "bot-token",
                    "userId": "user-1",
                    "contextTokens": {"user-1": "ctx-1"},
                }
            ),
            encoding="utf-8",
        )
        transcript = tmp / "session-1.jsonl"
        transcript.write_text(
            "\n".join(
                [
                    json.dumps({"type": "ai-title", "aiTitle": "旧标题", "sessionId": "sess-1"}),
                    json.dumps({"type": "custom-title", "customTitle": "我的会话", "sessionId": "sess-1"}),
                ]
            ),
            encoding="utf-8",
        )
        result = subprocess.run(
            [PYTHON, "-I", str(SCRIPT)],
            input=json.dumps(
                {
                    "session_id": "sess-1",
                    "cwd": "D:\\work\\app",
                    "transcript_path": str(transcript),
                    "last_assistant_message": "修复了登录失败。",
                },
                ensure_ascii=False,
            ).encode("utf-8"),
            capture_output=True,
            env={
                "SYSTEMROOT": "C:\\Windows",
                "APPDATA": str(tmp / "missing-roaming"),
                "WARP_WECHAT_CONFIG_DIR": str(config_dir),
            },
            check=False,
        )
        assert result.returncode == 0, result.stderr.decode("utf-8", "replace")
        assert server.requests, result.stderr.decode("utf-8", "replace")
        text = server.requests[0]["body"]["msg"]["item_list"][0]["text_item"]["text"]
        assert "Claude Code 已完成" in text
        assert "我的会话" in text
        assert "修复了登录失败。" in text
        assert "sess-1" not in text
        assert len(text) < 220
        assert b"bot-token" not in result.stderr
    finally:
        server.shutdown()


def test_hook_skips_reentrant_stop(tmp: Path) -> None:
    result = subprocess.run(
        [PYTHON, "-I", str(SCRIPT)],
        input=b'{"stop_hook_active": true}',
        capture_output=True,
        env={"SYSTEMROOT": "C:\\Windows", "WARP_WECHAT_CONFIG_DIR": str(tmp)},
        check=False,
    )
    assert result.returncode == 0
    assert result.stderr == b""


def test_chunk_text_keeps_content() -> None:
    text = f"{'a' * 120}\n{'b' * 120}"
    chunks = wechat_ilink.chunk_text(text, 150)
    assert "\n".join(chunks) == text
    assert all(len(chunk) <= 150 for chunk in chunks)


def main() -> None:
    tmp = ROOT / ".notify-test-tmp"
    tmp.mkdir(exist_ok=True)
    try:
        test_send_text_uses_ilink_shape()
        test_send_text_reports_ilink_error()
        test_chunk_text_keeps_content()
        test_imports_legacy_config_once(tmp, os.environ)
        test_hook_posts_completion(tmp)
        test_hook_skips_reentrant_stop(tmp)
    finally:
        for child in sorted(tmp.rglob("*"), reverse=True):
            if child.is_file():
                child.unlink(missing_ok=True)
            elif child.is_dir():
                child.rmdir()
        tmp.rmdir()
    print("ok")


if __name__ == "__main__":
    main()
