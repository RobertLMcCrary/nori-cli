"""Time the Claude CLI in stream-json mode, the way the claude-code-agent-sdk driver runs it.

usage:
  python3 -I cli_floor_probe.py --runs 5 --model haiku [--prompt "Reply with just OK"]
  python3 -I cli_floor_probe.py --runs 5 --mcp-sdk --no-prompt

Per run, in a fresh temp cwd:
  spawn -> first stdout line            (first_line)
  spawn -> system/init                  (init)            [if emitted before the prompt]
  spawn -> control_response(initialize) (initialize)      the SDK's handshake
  initialize -> user message written    (write, ~0)
  user message -> first assistant text  (first_text)      the time-to-first-token floor
  user message -> result                (result)

Spawn arguments mirror claude-code-agent-sdk 0.1.39 (subprocess.rs build_command):
  --output-format stream-json --verbose --input-format stream-json --model M
  --permission-prompt-tool stdio   (plus --mcp-config for --mcp-sdk)
Stdlib only. Prints one JSON object per run and a median/p90 summary.
"""

import argparse
import json
import os
import queue
import shutil
import statistics
import subprocess
import tempfile
import threading
import time


def spawn(cli, model, mcp_sdk, cwd):
    args = [
        cli,
        "--output-format",
        "stream-json",
        "--verbose",
        "--input-format",
        "stream-json",
        "--model",
        model,
        "--permission-prompt-tool",
        "stdio",
    ]
    if mcp_sdk:
        args += ["--mcp-config", json.dumps({"mcpServers": {"nori": {"type": "sdk", "name": "nori"}}})]
    env = dict(os.environ, CLAUDE_CODE_ENTRYPOINT="sdk-rust", CLAUDE_AGENT_SDK_VERSION="0.1.39")
    return subprocess.Popen(
        args,
        cwd=cwd,
        env=env,
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def pump(proc, q, t0):
    for raw in proc.stdout:
        q.put(((time.monotonic() - t0) * 1000.0, raw))
    q.put((None, None))


def wait_for(q, pred, deadline):
    """Return (ms, obj) for the first line matching pred, or (None, None) on timeout/EOF."""
    while True:
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return None, None
        try:
            ms, raw = q.get(timeout=remaining)
        except queue.Empty:
            return None, None
        if raw is None:
            return None, None
        try:
            obj = json.loads(raw)
        except ValueError:
            continue
        if pred(obj):
            return ms, obj


def run_once(args, cwd):
    t0 = time.monotonic()
    proc = spawn(args.cli, args.model, args.mcp_sdk, cwd)
    q = queue.Queue()
    threading.Thread(target=pump, args=(proc, q, t0), daemon=True).start()
    out = {}
    deadline = time.monotonic() + args.timeout

    # First stdout line of any kind, then the SDK handshake.
    proc.stdin.write(
        (json.dumps({"type": "control_request", "request_id": "req_1", "request": {"subtype": "initialize", "hooks": None}}) + "\n").encode()
    )
    proc.stdin.flush()
    first_ms = None
    init_ms = None
    init_resp_ms = None
    while init_resp_ms is None:
        ms, obj = wait_for(q, lambda o: True, deadline)
        if obj is None:
            break
        if first_ms is None:
            first_ms = ms
        if obj.get("type") == "system" and obj.get("subtype") == "init" and init_ms is None:
            init_ms = ms
        if obj.get("type") == "control_response":
            init_resp_ms = ms
    out["first_line"] = first_ms
    out["init"] = init_ms
    out["initialize"] = init_resp_ms

    if args.no_prompt or init_resp_ms is None:
        out["timed_out"] = init_resp_ms is None
        proc.kill()
        return out

    # The prompt, as the SDK writes it.
    send_ms = (time.monotonic() - t0) * 1000.0
    msg = {
        "type": "user",
        "message": {"role": "user", "content": args.prompt},
        "parent_tool_use_id": None,
        "session_id": "",
    }
    proc.stdin.write((json.dumps(msg) + "\n").encode())
    proc.stdin.flush()
    deadline = time.monotonic() + args.timeout
    text_ms, _ = wait_for(
        q,
        lambda o: o.get("type") == "assistant"
        and any(block.get("type") == "text" for block in o.get("message", {}).get("content", [])),
        deadline,
    )
    result_ms, _ = wait_for(q, lambda o: o.get("type") == "result", deadline)
    out["send"] = send_ms
    out["first_text"] = None if text_ms is None else text_ms - send_ms
    out["result"] = None if result_ms is None else result_ms - send_ms
    proc.kill()
    return out


def pct(values, p):
    values = sorted(values)
    if not values:
        return None
    k = max(0, min(len(values) - 1, round(p / 100.0 * (len(values) - 1))))
    return values[k]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cli", default="claude")
    ap.add_argument("--model", default="haiku")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--prompt", default="Reply with just OK")
    ap.add_argument("--mcp-sdk", action="store_true")
    ap.add_argument("--no-prompt", action="store_true", help="stop after initialize (no model call)")
    args = ap.parse_args()

    runs = []
    for i in range(args.runs):
        cwd = tempfile.mkdtemp(prefix="cli-floor-")
        try:
            r = run_once(args, cwd)
        finally:
            shutil.rmtree(cwd, ignore_errors=True)
        r["run"] = i + 1
        runs.append(r)
        print(json.dumps(r), flush=True)

    print("--- summary (ms) ---")
    for key in ["first_line", "init", "initialize", "first_text", "result"]:
        vals = [r[key] for r in runs if r.get(key) is not None]
        if not vals:
            print(f"{key}: no samples")
            continue
        print(
            f"{key}: n={len(vals)} median={statistics.median(vals):.0f} p90={pct(vals, 90):.0f} "
            f"min={min(vals):.0f} max={max(vals):.0f}"
        )


if __name__ == "__main__":
    main()
