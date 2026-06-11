#!/usr/bin/env python3
"""
ulcli - Command-line interface for the ULMEN ecosystem.

Usage:
    ulcli health
    ulcli tools
    ulcli chat "What is 2+2?"
    ulcli run "review auth code"
    ulcli session demo_1 "Hi, I'm Mehdi"
    ulcli search "validate token"
    ulcli put key value
    ulcli get key
    ulcli dashboard
    ulcli metrics
    ulcli runs
    ulcli workflows

Environment:
    ULGATE_URL      server URL (default: http://localhost:8080)
    ULGATE_API_KEY   API key for auth
"""

import json
import os
import sys
import urllib.request
import urllib.error
import urllib.parse


URL = os.environ.get("ULGATE_URL", "http://localhost:8080")
KEY = os.environ.get("ULGATE_API_KEY", "")


def headers():
    h = {"Content-Type": "application/json"}
    if KEY:
        h["Authorization"] = f"Bearer {KEY}"
    return h


def get(path):
    req = urllib.request.Request(f"{URL}{path}", headers=headers())
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        return json.loads(e.read().decode())


def post(path, body):
    data = json.dumps(body).encode()
    req = urllib.request.Request(f"{URL}{path}", data=data, headers=headers(), method="POST")
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return json.loads(resp.read().decode())
    except urllib.error.HTTPError as e:
        return json.loads(e.read().decode())


def stream(path, body):
    data = json.dumps(body).encode()
    req = urllib.request.Request(f"{URL}{path}", data=data, headers=headers(), method="POST")
    try:
        with urllib.request.urlopen(req, timeout=120) as resp:
            for line in resp:
                line = line.decode().strip()
                if line.startswith("data: ") and line != "data: [DONE]":
                    try:
                        event = json.loads(line[6:])
                        if event.get("type") == "token":
                            print(event.get("content", ""), end="", flush=True)
                        elif event.get("type") == "done":
                            print()
                            return event
                        elif event.get("type") == "error":
                            print(f"\nError: {event.get('message')}", file=sys.stderr)
                            return event
                    except json.JSONDecodeError:
                        pass
    except urllib.error.HTTPError as e:
        print(f"Error: {e.read().decode()}", file=sys.stderr)
        sys.exit(1)


def pp(data):
    print(json.dumps(data, indent=2))


def main():
    args = sys.argv[1:]
    if not args or args[0] in ("--help", "-h", "help"):
        print(__doc__)
        return

    cmd = args[0]

    if cmd == "health":
        pp(get("/v1/health"))

    elif cmd == "tools":
        d = get("/v1/tools")
        for t in d.get("tools", []):
            params = ", ".join(p["name"] for p in t.get("params", []))
            print(f"  {t['name']:20s} {t.get('description','')}  ({params})")

    elif cmd == "chat":
        msg = " ".join(args[1:]) if len(args) > 1 else input("Message: ")
        print()
        stream("/v1/chat/stream", {"message": msg})

    elif cmd == "run":
        task = " ".join(args[1:]) if len(args) > 1 else input("Task: ")
        result = post("/v1/run", {"input": {"task": task}})
        if result.get("status") == "succeeded":
            for key, val in result.get("outputs", {}).items():
                print(f"\n--- {key} ---")
                print(val[:2000] if isinstance(val, str) else val)
            print(f"\n[{result.get('tokens_used',0)} tokens, {result.get('latency_ms',0)}ms]")
        else:
            print(f"Error: {result.get('error', result)}", file=sys.stderr)

    elif cmd == "session":
        if len(args) < 3:
            print("Usage: ulcli session <id> <message>", file=sys.stderr)
            sys.exit(1)
        sid = args[1]
        msg = " ".join(args[2:])
        result = post(f"/v1/sessions/{sid}/message", {"message": msg})
        print(result.get("content", result))

    elif cmd == "search":
        q = " ".join(args[1:]) if len(args) > 1 else input("Query: ")
        d = get(f"/v1/db/search?q={urllib.parse.quote(q)}")
        for r in d.get("results", []):
            print(f"  {r['key']:40s} score={r['score']:.4f}")
            if r.get("content"):
                print(f"    {r['content'][:100]}")

    elif cmd == "put":
        if len(args) < 3:
            print("Usage: ulcli put <key> <value>", file=sys.stderr)
            sys.exit(1)
        pp(post("/v1/db/put", {"key": args[1], "value": " ".join(args[2:])}))

    elif cmd == "get":
        if len(args) < 2:
            print("Usage: ulcli get <key>", file=sys.stderr)
            sys.exit(1)
        d = get(f"/v1/db/get?key={args[1]}")
        if "value" in d:
            print(d["value"])
        else:
            print(f"Not found: {args[1]}", file=sys.stderr)

    elif cmd == "dashboard":
        d = get("/v1/dashboard")
        print(f"Status:    {d.get('status')}")
        print(f"LLM:       {d.get('llm')}")
        print(f"Uptime:    {d.get('uptime_seconds')}s")
        print(f"Tools:     {d.get('tools')}")
        s = d.get("stats", {})
        print(f"Runs:      {s.get('total_runs', 0)}")
        print(f"Tokens:    {s.get('total_tokens', 0)}")
        print(f"Sessions:  {s.get('active_sessions', 0)}")
        print(f"Workflows: {s.get('registered_workflows', 0)}")
        if d.get("recent_runs"):
            print(f"\nRecent runs:")
            for r in d["recent_runs"]:
                print(f"  {r.get('run_id','?'):30s} {r.get('status','?'):10s} {r.get('tokens',0):>6} tok  {r.get('latency_ms',0):>6}ms")

    elif cmd == "metrics":
        d = get("/v1/metrics")
        print(f"Runs:         {d.get('total_runs')}")
        print(f"Success:      {d.get('success_rate')}")
        print(f"Tokens:       {d.get('total_tokens')}")
        print(f"Tokens/min:   {d.get('tokens_per_minute')}")
        print(f"Runs/min:     {d.get('runs_per_minute')}")
        lat = d.get("latency", {})
        print(f"Latency avg:  {lat.get('avg_ms')}ms")
        print(f"Latency p50:  {lat.get('p50_ms')}ms")
        print(f"Latency p95:  {lat.get('p95_ms')}ms")

    elif cmd == "runs":
        d = get("/v1/runs")
        for r in d.get("runs", []):
            print(f"  {r.get('run_id','?'):30s} {r.get('status','?'):10s} {r.get('tokens_used',0):>6} tok  {r.get('latency_ms',0):>6}ms  {r.get('workflow','?')}")

    elif cmd == "workflows":
        d = get("/v1/workflows")
        for w in d.get("workflows", []):
            print(f"  {w.get('name','?'):20s} {w.get('steps',0)} steps")

    else:
        print(f"Unknown command: {cmd}", file=sys.stderr)
        print("Run 'ulcli help' for usage", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
