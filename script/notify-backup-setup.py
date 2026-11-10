#!/usr/bin/env python3
"""Configure and test the Bark backup notification channel.

Interactive:
    python -I script/notify-backup-setup.py

Non-interactive:
    python -I script/notify-backup-setup.py --bark <key>

Bark: install the App (App Store 搜 Bark), copy the key from the app's
homepage URL (https://api.day.app/<这一段就是key>).
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import notify_backup


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bark", help="Bark key（App 首页 URL 里 api.day.app/ 后面那段）")
    parser.add_argument("--skip-test", action="store_true", help="只写配置不发测试")
    args = parser.parse_args()

    bark_key = args.bark or input("Bark key（直接回车跳过）: ").strip()
    bark_key = bark_key.strip().removeprefix("https://api.day.app/").rstrip("/")
    if not bark_key:
        print("没有可写入的配置，退出。")
        return 1

    config = {"barkUrl": f"https://api.day.app/{bark_key}"}
    path = notify_backup.backup_config_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(config, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"已写入 {path}")

    if args.skip_test:
        return 0
    try:
        notify_backup.send_bark("Warp 备份通道测试 ✅")
        print("Bark: 测试消息已发送，请查收 iPhone 通知")
    except Exception as err:
        print(f"Bark: 测试失败 — {err}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
