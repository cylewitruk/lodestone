#!/usr/bin/env python3
"""Minimal stdio MCP client for `ck --serve`. Usage: ck_mcp.py <ck-bin> <cwd> <tool> '<json args>' [timeout_s]"""
import json
import subprocess
import sys
import time


def main() -> None:
    ck, cwd, tool, args = sys.argv[1:5]
    timeout = float(sys.argv[5]) if len(sys.argv) > 5 else 3600
    proc = subprocess.Popen([ck, "--serve"], cwd=cwd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                            stderr=subprocess.DEVNULL, text=True)

    def send(msg: dict) -> None:
        proc.stdin.write(json.dumps(msg) + "\n")
        proc.stdin.flush()

    def recv(want_id: int) -> dict:
        deadline = time.time() + timeout
        while time.time() < deadline:
            line = proc.stdout.readline()
            if not line:
                raise SystemExit("server closed")
            try:
                msg = json.loads(line)
            except json.JSONDecodeError:
                continue
            if msg.get("id") == want_id:
                return msg
        raise SystemExit("timeout")

    send({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "probe", "version": "0"}}})
    recv(1)
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    t0 = time.time()
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": tool, "arguments": json.loads(args)}})
    resp = recv(2)
    wall = time.time() - t0
    proc.stdin.close()
    proc.terminate()
    content = resp.get("result", {}).get("content", [])
    text = content[0].get("text", "") if content else json.dumps(resp)[:500]
    try:
        body = json.loads(text)
        meta = body.get("metadata")
        hits = [m.get("file") or m.get("path") for m in body.get("results", {}).get("matches", [])]
        print(json.dumps({"wall_s": round(wall, 1), "metadata": meta, "hits": hits[:8]}))
    except json.JSONDecodeError:
        print(json.dumps({"wall_s": round(wall, 1), "text": text[:600]}))


if __name__ == "__main__":
    main()
