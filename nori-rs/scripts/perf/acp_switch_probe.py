"""Time Nori's ACP facade the way an editor (or the TUI's agent switch) drives it.

usage:
  python3 -I acp_switch_probe.py --nori target/debug/nori --agent claude \
      --home $H --runs 5 --prompt "Reply with just OK" [--second-prompt]
  python3 -I acp_switch_probe.py --nori target/debug/nori --agent elizacp --home $H --runs 5

Spawns `nori exec --acp --agent AGENT -C <tmp> --dangerously-bypass-approvals-and-sandbox`
with NORI_HOME set to --home, speaks newline-delimited JSON-RPC 2.0 over its stdio, and times,
per round:
  initialize              request -> response
  session/new             request -> response              (first session)
  first_chunk             session/prompt -> first session/update with agent_message_chunk
  prompt_done             session/prompt -> its response
  switch_new              second session/new on the same connection (approximates a switch)
  switch_first_chunk      (--second-prompt) second session/new -> first agent_message_chunk
Stdlib only. Prints one JSON object per round and a median/p90 summary.
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


class Acp:
    def __init__(self, cmd, env, cwd):
        self.t0 = time.monotonic()
        self.proc = subprocess.Popen(
            cmd, cwd=cwd, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE
        )
        self.inbox = queue.Queue()
        self.pending = {}
        self.next_id = 1
        self.lock = threading.Lock()
        threading.Thread(target=self._read, daemon=True).start()
        threading.Thread(target=self._drain_stderr, daemon=True).start()
        self.stderr_lines = []

    def _drain_stderr(self):
        for raw in self.proc.stderr:
            self.stderr_lines.append(raw.decode(errors="replace").rstrip())

    def _now_ms(self):
        return (time.monotonic() - self.t0) * 1000.0

    def _read(self):
        for raw in self.proc.stdout:
            ms = self._now_ms()
            try:
                msg = json.loads(raw)
            except ValueError:
                continue
            if "id" in msg and ("result" in msg or "error" in msg) and "method" not in msg:
                with self.lock:
                    slot = self.pending.pop(msg["id"], None)
                if slot is not None:
                    slot["ms"] = ms
                    slot["msg"] = msg
                    slot["event"].set()
                continue
            if "method" in msg and "id" in msg:
                # Agent -> client request (e.g. session/request_permission): refuse, do not hang.
                self._send({"jsonrpc": "2.0", "id": msg["id"], "error": {"code": -32601, "message": "not supported by probe"}})
                continue
            self.inbox.put((ms, msg))
        self.inbox.put((None, None))

    def _send(self, obj):
        self.proc.stdin.write((json.dumps(obj) + "\n").encode())
        self.proc.stdin.flush()

    def request(self, method, params, timeout):
        """Send a request and block for its response. Returns (sent_ms, response_ms, msg)."""
        event = threading.Event()
        slot = {"event": event}
        with self.lock:
            rid = self.next_id
            self.next_id += 1
            self.pending[rid] = slot
        sent = self._now_ms()
        self._send({"jsonrpc": "2.0", "id": rid, "method": method, "params": params})
        if not event.wait(timeout):
            raise TimeoutError(f"{method} timed out after {timeout}s")
        if "error" in slot["msg"]:
            raise RuntimeError(f"{method} error: {json.dumps(slot['msg']['error'])[:300]}")
        return sent, slot["ms"], slot["msg"]

    def wait_update(self, pred, timeout):
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            try:
                ms, msg = self.inbox.get(timeout=remaining)
            except queue.Empty:
                return None
            if msg is None:
                return None
            if msg.get("method") == "session/update" and pred(msg["params"]):
                return ms

    def drain(self):
        """Discard queued notifications so the next wait sees only fresh ones."""
        while True:
            try:
                self.inbox.get_nowait()
            except queue.Empty:
                return

    def close(self):
        try:
            self.proc.stdin.close()
        except OSError:
            pass
        try:
            self.proc.wait(timeout=5)
        except subprocess.TimeoutExpired:
            self.proc.kill()


def is_text_chunk(params):
    update = params.get("update", {})
    return update.get("sessionUpdate") == "agent_message_chunk"


def run_round(args, home, env_base):
    cwd = tempfile.mkdtemp(prefix="acp-switch-")
    env = dict(env_base, NORI_HOME=home)
    cmd = [
        args.nori,
        "exec",
        "--acp",
        "--agent",
        args.agent,
        "-C",
        cwd,
        "--dangerously-bypass-approvals-and-sandbox",
    ]
    out = {}
    acp = Acp(cmd, env, cwd)
    try:
        t = args.timeout
        sent, got, _ = acp.request(
            "initialize", {"protocolVersion": 1, "clientCapabilities": {}, "clientInfo": {"name": "acp-switch-probe", "version": "0"}}, t
        )
        out["initialize"] = got - sent

        sent, got, resp = acp.request("session/new", {"cwd": cwd, "mcpServers": []}, t)
        out["session_new"] = got - sent
        session_id = resp["result"]["sessionId"]

        if args.no_prompt:
            return out

        acp.drain()
        sent = acp._now_ms()
        prompt_text = args.prompt
        # Start the prompt without blocking, then time its first chunk and its response.
        prompt_event = threading.Event()
        slot = {"event": prompt_event}
        with acp.lock:
            pid = acp.next_id
            acp.next_id += 1
            acp.pending[pid] = slot
        acp._send(
            {
                "jsonrpc": "2.0",
                "id": pid,
                "method": "session/prompt",
                "params": {"sessionId": session_id, "prompt": [{"type": "text", "text": prompt_text}]},
            }
        )
        first = acp.wait_update(is_text_chunk, t)
        out["first_chunk"] = None if first is None else first - sent
        if not prompt_event.wait(t):
            raise TimeoutError("session/prompt did not finish")
        out["prompt_done"] = slot["ms"] - sent

        # Switch approximation: a second session/new on the same connection. The exec facade
        # holds one session per connection, so this is expected to be refused; the error is recorded.
        try:
            sent, got, resp2 = acp.request("session/new", {"cwd": cwd, "mcpServers": []}, t)
            out["switch_new"] = got - sent
            session_id2 = resp2["result"]["sessionId"]
        except RuntimeError as exc:
            out["switch_new_error"] = str(exc)
            session_id2 = None

        if args.second_prompt and session_id2 is not None:
            acp.drain()
            sent = acp._now_ms()
            second_event = threading.Event()
            slot2 = {"event": second_event}
            with acp.lock:
                pid2 = acp.next_id
                acp.next_id += 1
                acp.pending[pid2] = slot2
            acp._send(
                {
                    "jsonrpc": "2.0",
                    "id": pid2,
                    "method": "session/prompt",
                    "params": {"sessionId": session_id2, "prompt": [{"type": "text", "text": prompt_text}]},
                }
            )
            first2 = acp.wait_update(is_text_chunk, t)
            out["switch_first_chunk"] = None if first2 is None else first2 - sent
            second_event.wait(t)
    except (TimeoutError, OSError, RuntimeError) as exc:
        out["error"] = str(exc)
        out["stderr_tail"] = acp.stderr_lines[-5:]
    finally:
        acp.close()
        shutil.rmtree(cwd, ignore_errors=True)
    return out


def pct(values, p):
    values = sorted(values)
    k = max(0, min(len(values) - 1, round(p / 100.0 * (len(values) - 1))))
    return values[k]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nori", default="target/debug/nori")
    ap.add_argument("--agent", default="claude")
    ap.add_argument("--home", required=True, help="NORI_HOME for the child")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--timeout", type=float, default=120.0)
    ap.add_argument("--prompt", default="Reply with just OK")
    ap.add_argument("--second-prompt", action="store_true")
    ap.add_argument("--no-prompt", action="store_true", help="stop after session/new (no model call)")
    args = ap.parse_args()

    results = []
    for i in range(args.runs):
        r = run_round(args, args.home, dict(os.environ))
        r["run"] = i + 1
        results.append(r)
        print(json.dumps(r), flush=True)

    print("--- summary (ms) ---")
    keys = ["initialize", "session_new", "first_chunk", "prompt_done", "switch_new", "switch_first_chunk"]
    for key in keys:
        vals = [r[key] for r in results if r.get(key) is not None]
        if not vals:
            print(f"{key}: no samples")
            continue
        print(
            f"{key}: n={len(vals)} median={statistics.median(vals):.0f} p90={pct(vals, 90):.0f} "
            f"min={min(vals):.0f} max={max(vals):.0f}"
        )


if __name__ == "__main__":
    main()
