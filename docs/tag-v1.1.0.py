#!/usr/bin/env python3
"""为 chatgpt-installer 打 v1.1.0 tag（触发 GitHub Actions 构建 + 自动创建 Release）。

用法（需用户拍板后执行）:
    python3 docs/tag-v1.1.0.py

机制: 沿用 local-github-push 的 PAT 直连模式 ——
经 dynamic_credentials 占位符由 authd 在出口处替换为真实 token，
脚本本身永远接触不到 token 明文。认证走 credentials.request_api_access
建好的 custom.github-pat 连接器。
"""
import json
import sys
import urllib.request
import urllib.error

sys.path.insert(0, "/opt/muse/skills/custom-connector-setup/templates/skill/bin")
from dynamic_credentials import add_surrogate_to_request, read_json_response

CRED = "custom.github-pat"
HOSTS = ["api.github.com"]
BASE = "https://api.github.com"
OWNER, REPO = "zktww", "chatgpt-installer"
TAG = "v1.1.0"


def api(method, path, payload=None):
    req = urllib.request.Request(
        BASE + path,
        data=json.dumps(payload).encode() if payload is not None else None,
        method=method,
        headers={"Accept": "application/vnd.github+json", "Content-Type": "application/json"},
    )
    add_surrogate_to_request(req, CRED, allowed_hosts=HOSTS)
    with urllib.request.urlopen(req, timeout=60) as resp:
        return read_json_response(resp)


def main():
    # 1. 取 main 分支当前 HEAD
    main_ref = api("GET", f"/repos/{OWNER}/{REPO}/git/refs/heads/main")
    head_sha = main_ref["object"]["sha"]
    print(f"main HEAD: {head_sha}")

    # 2. 检查 tag 是否已存在（幂等）
    try:
        existing = api("GET", f"/repos/{OWNER}/{REPO}/git/refs/tags/{TAG}")
        print(f"tag {TAG} 已存在 -> {existing['object']['sha']}，不重复创建")
        return
    except urllib.error.HTTPError as e:
        if e.code != 404:
            raise
        # 404 = tag 不存在，继续创建

    # 3. 创建 tag（轻量 tag，CI 的 push:tags 'v*' 触发器认它）
    created = api(
        "POST",
        f"/repos/{OWNER}/{REPO}/git/refs",
        {"ref": f"refs/tags/{TAG}", "sha": head_sha},
    )
    print(f"已创建 {created['ref']} -> {created['object']['sha']}")
    print("GitHub Actions 构建已触发，Release（含 exe + NSIS）将自动生成")


if __name__ == "__main__":
    main()
