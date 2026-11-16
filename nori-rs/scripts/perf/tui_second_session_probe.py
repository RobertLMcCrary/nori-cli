"""Measure a second session's prompt latency in one nori TUI process (the warm-session case).

usage:
  NORI_HOME=$H python3 -I tui_second_session_probe.py --nori target/debug/nori [--pause 5]

Starts the TUI in a pty with the claude agent, sends PROMPT, waits until the driver logs the
prompt as completed, waits PAUSE seconds, sends /new, sends PROMPT again, and waits for that
completion. Timings come from the driver's own log (NORI_HOME/log/nori-acp.*.log), so no screen
parsing: per session, "Creating new ACP session" -> "Sending query to Claude CLI" -> "Completed
prompt task", plus any "Took a warm Claude session" line. Set CLAUDE_ACP_NO_WARM=1 in the
environment to compare with the pool off.

Answers DSR and DA1 queries; other terminal queries time out harmlessly.
"""

import argparse
import datetime
import glob
import os
import pty
import re
import select
import struct
import fcntl
import termios
import time
import subprocess

TS = re.compile(r"^(\d{4}-\d\d-\d\dT[\d:.]+)Z")


def ts(line):
    m = TS.match(line)
    return datetime.datetime.fromisoformat(m.group(1)) if m else None


def log_lines(home):
    files = sorted(glob.glob(os.path.join(home, "log", "nori-*.log")))
    lines = []
    for f in files:
        with open(f, errors="replace") as fh:
            lines += fh.read().splitlines()
    return lines


def submit(fd, text, state):
    """Type text, then press Enter as its own keystroke so the composer does not swallow it."""
    os.write(fd, text.encode())
    drain(fd, 0.7, state)
    os.write(fd, b"\r")


def drain(fd, seconds, state):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if not r:
            continue
        try:
            data = os.read(fd, 65536)
        except OSError:
            return
        if not data:
            return
        if b"\x1b[6n" in data:
            os.write(fd, b"\x1b[1;1R")
        if b"\x1b[c" in data:
            os.write(fd, b"\x1b[?62;c")


def wait_for_completions(fd, home, want, timeout, state):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        drain(fd, 0.5, state)
        done = sum("Completed prompt" in line for line in log_lines(home))
        if done >= want:
            return True
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--nori", required=True)
    ap.add_argument("--pause", type=float, default=5.0)
    ap.add_argument("--prompt", default="Reply with just OK")
    ap.add_argument("--timeout", type=float, default=120.0)
    args = ap.parse_args()
    home = os.environ["NORI_HOME"]
    cwd = os.environ.get("PROBE_CWD", home)

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    proc = subprocess.Popen(
        [args.nori, "--agent", "claude", "--skip-welcome", "--skip-trust-directory",
         "--dangerously-bypass-approvals-and-sandbox"],
        stdin=slave, stdout=slave, stderr=slave, cwd=cwd, close_fds=True,
        env=dict(os.environ, TERM="xterm-256color"),
    )
    os.close(slave)
    state = {}
    drain(master, 12, state)  # startup: composer, agent connect

    t_first = time.monotonic()
    submit(master, args.prompt, state)
    ok1 = wait_for_completions(master, home, 1, args.timeout, state)
    time.sleep(0)
    drain(master, args.pause, state)  # idle gap: the warm pool may refill here

    before = len(log_lines(home))
    t_new = time.monotonic()
    submit(master, "/new", state)
    drain(master, 3, state)
    submit(master, args.prompt, state)
    ok2 = wait_for_completions(master, home, 2, args.timeout, state)
    proc.kill()

    lines = log_lines(home)[before:]
    print(f"first_session_completed={ok1} second_session_completed={ok2}")
    marks = {"new": "Creating new ACP session", "send": "Sending query to Claude CLI",
             "done": "Completed prompt task", "warm": "Took a warm Claude session"}
    seen = {}
    for line in lines:
        for k, m in marks.items():
            if m in line:
                seen.setdefault(k, []).append(ts(line))
    for k in marks:
        print(k, [str(v) for v in seen.get(k, [])])
    if seen.get("new") and seen.get("done"):
        new = seen["new"][-1]
        done = seen["done"][-1]
        send = seen.get("send", [None])[-1]
        print("second_session_new_to_done_ms", round((done - new).total_seconds() * 1000))
        if send:
            print("second_session_send_to_done_ms", round((done - send).total_seconds() * 1000))


if __name__ == "__main__":
    main()
