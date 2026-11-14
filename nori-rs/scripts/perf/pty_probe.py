"""Drive nori in a pty and print a millisecond timeline plus a latency summary.

usage:
  NORI_HOME=$H PROBE_SECS=7 PROBE_PROMPT="hello there" PROBE_TYPE_AT=2.0 \
    python3 -I pty_probe.py target/debug/nori --agent mock-model --skip-welcome \
      --skip-trust-directory --dangerously-bypass-approvals-and-sandbox

Answers the terminal queries nori sends at startup (DSR, kitty keyboard, DA1,
OSC 10/11) so no query waits on a timeout. After the composer glyph appears and
PROBE_TYPE_AT seconds have passed, types PROBE_PROMPT and presses Enter, then
logs every output chunk with the time since Enter.

Environment knobs (all optional):
  PROBE_SECS            total run time (default 6)
  PROBE_PROMPT          prompt to type and submit
  PROBE_TYPE_AT         seconds after launch before typing (default 2.0)
  PROBE_ROWS/COLS       pty size (default 40x120)
  PROBE_STREAM          "tokens" or "lines": the mock agent streams a generated
                        file (needs the mock agent, i.e. --agent mock-model)
  PROBE_STREAM_COUNT    tokens (default 400) or lines (default 2000)
  PROBE_STREAM_CHUNK    bytes per agent_message_chunk (default 8)
  PROBE_STREAM_DELAY_MS delay between chunks in the agent (default 0)
  PROBE_CTRL_L_AT       seconds after Enter to send Ctrl+L (default: off)
  PROBE_VERBOSE         1 to print every output chunk after Enter
  PROBE_DUMP            path to dump the raw output bytes to

The summary lines are numbered:
  1 Enter->echo, 2 Enter->session/prompt (agent clock), 3 delta->paint,
  4 burst drain, 6 pty reads per synchronized frame (a lower bound on write(2)
  calls: several writes can be coalesced into one read), 7 first frame and agent
  spawn, 8 Ctrl+L latency.
"""

import fcntl
import os
import pty
import re
import select
import signal
import struct
import sys
import tempfile
import termios
import time

STREAM_ID = re.compile(r"\b([TL]\d{5})\b")
ANSI = re.compile(
    rb"\x1b\[[0-9;?<>=:]*[ -/]*[a-zA-Z@`]|\x1b\][^\x07\x1b]*(\x07|\x1b\\)|\x1b[=>M78]"
)
SYNC_BEGIN = b"\x1b[?2026h"
SYNC_END = b"\x1b[?2026l"
QUERIES = [
    (b"\x1b[6n", b"\x1b[1;1R", "DSR"),
    (b"\x1b[?u", b"\x1b[?0u", "kitty-kbd"),
    (b"\x1b[c", b"\x1b[?62;c", "DA1"),
    (b"\x1b]10;?", b"\x1b]10;rgb:ffff/ffff/ffff\x1b\\", "osc10"),
    (b"\x1b]11;?", b"\x1b]11;rgb:0000/0000/0000\x1b\\", "osc11"),
]
TOKEN_WIDTH = 7  # "T00012 "


class Screen:
    """A small VT100 screen model: enough of the protocol to see what a TUI shows.

    Handles cursor movement, erasing, scroll regions, reverse index, save/restore cursor, wide
    characters and deferred autowrap. Rows pushed off the top of a full-screen region go to
    `scrollback`. Anything else (colors, modes, OSC) is parsed and ignored.
    """

    def __init__(self, rows, cols):
        import codecs

        self.rows, self.cols = rows, cols
        self.grid = [[" "] * cols for _ in range(rows)]
        self.scrollback = []
        self.cx = self.cy = 0
        self.saved = (0, 0)
        self.top, self.bottom = 0, rows - 1
        self.pending_wrap = False
        self.state = "ground"
        self.params = ""
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.bytes_2j = self.bytes_3j = 0

    def row_text(self, y):
        return "".join(self.grid[y]).rstrip()

    def screen_text(self):
        return [self.row_text(y) for y in range(self.rows)]

    def _scroll_up(self, n=1):
        for _ in range(n):
            row = self.grid[self.top]
            if self.top == 0:
                self.scrollback.append("".join(row).rstrip())
            del self.grid[self.top]
            self.grid.insert(self.bottom, [" "] * self.cols)

    def _scroll_down(self, n=1):
        for _ in range(n):
            del self.grid[self.bottom]
            self.grid.insert(self.top, [" "] * self.cols)

    def _linefeed(self):
        if self.cy == self.bottom:
            self._scroll_up()
        elif self.cy < self.rows - 1:
            self.cy += 1

    def _put(self, ch):
        import unicodedata

        width = 0 if unicodedata.combining(ch) else (2 if unicodedata.east_asian_width(ch) in "WF" else 1)
        if width == 0:
            return
        if self.pending_wrap or self.cx + width > self.cols:
            self.cx = 0
            self._linefeed()
            self.pending_wrap = False
        self.grid[self.cy][self.cx] = ch
        if width == 2 and self.cx + 1 < self.cols:
            self.grid[self.cy][self.cx + 1] = ""
        self.cx += width
        if self.cx >= self.cols:
            self.cx = self.cols - 1
            self.pending_wrap = True

    def feed(self, data):
        for ch in self.decoder.decode(data):
            self._feed_char(ch)

    def _feed_char(self, ch):
        st = self.state
        if st == "ground":
            if ch == "\x1b":
                self.state = "esc"
            elif ch == "\r":
                self.cx = 0
                self.pending_wrap = False
            elif ch == "\n":
                self._linefeed()
            elif ch == "\b":
                self.cx = max(0, self.cx - 1)
                self.pending_wrap = False
            elif ch == "\t":
                self.cx = min(self.cols - 1, (self.cx // 8 + 1) * 8)
            elif ch >= " " and ch != "\x7f":
                self._put(ch)
        elif st == "esc":
            if ch == "[":
                self.state, self.params = "csi", ""
            elif ch == "]":
                self.state = "osc"
            elif ch in "P_^X":
                self.state = "str"
            elif ch == "M":
                if self.cy == self.top:
                    self._scroll_down()
                else:
                    self.cy = max(0, self.cy - 1)
                self.state = "ground"
            elif ch == "7":
                self.saved = (self.cx, self.cy)
                self.state = "ground"
            elif ch == "8":
                self.cx, self.cy = self.saved
                self.state = "ground"
            else:
                self.state = "ground"
        elif st == "osc" or st == "str":
            if ch == "\x07":
                self.state = "ground"
            elif ch == "\x1b":
                self.state = "esc_st"
        elif st == "esc_st":
            self.state = "ground"
        elif st == "csi":
            if "\x40" <= ch <= "\x7e":
                self._csi(ch)
                self.state = "ground"
            else:
                self.params += ch

    def _csi(self, final):
        private = bool(self.params) and self.params[0] in "?<>="
        raw = self.params.lstrip("?<>=")
        nums = [int(p) if p.isdigit() else 0 for p in raw.split(";")] if raw else []
        n = nums[0] if nums and nums[0] else 1
        self.pending_wrap = False
        if private or final in "mhlnc":
            return
        if final == "A":
            self.cy = max(self.top if self.cy >= self.top else 0, self.cy - n)
        elif final == "B":
            self.cy = min(self.rows - 1, self.cy + n)
        elif final == "C":
            self.cx = min(self.cols - 1, self.cx + n)
        elif final == "D":
            self.cx = max(0, self.cx - n)
        elif final == "G":
            self.cx = min(self.cols - 1, n - 1)
        elif final == "d":
            self.cy = min(self.rows - 1, n - 1)
        elif final in "Hf":
            row = nums[0] if len(nums) > 0 and nums[0] else 1
            col = nums[1] if len(nums) > 1 and nums[1] else 1
            self.cy, self.cx = min(self.rows - 1, row - 1), min(self.cols - 1, col - 1)
        elif final == "J":
            mode = nums[0] if nums else 0
            if mode == 0:
                self._erase_line(self.cy, self.cx, self.cols)
                for y in range(self.cy + 1, self.rows):
                    self._erase_line(y, 0, self.cols)
            elif mode == 1:
                for y in range(self.cy):
                    self._erase_line(y, 0, self.cols)
                self._erase_line(self.cy, 0, self.cx + 1)
            else:
                for y in range(self.rows):
                    self._erase_line(y, 0, self.cols)
                if mode == 2:
                    self.bytes_2j += 1
                if mode == 3:
                    self.bytes_3j += 1
                    self.scrollback.clear()
        elif final == "K":
            mode = nums[0] if nums else 0
            if mode == 0:
                self._erase_line(self.cy, self.cx, self.cols)
            elif mode == 1:
                self._erase_line(self.cy, 0, self.cx + 1)
            else:
                self._erase_line(self.cy, 0, self.cols)
        elif final == "r":
            top = nums[0] if len(nums) > 0 and nums[0] else 1
            bottom = nums[1] if len(nums) > 1 and nums[1] else self.rows
            if top < bottom:
                self.top, self.bottom = top - 1, min(self.rows, bottom) - 1
                self.cx = self.cy = 0
        elif final == "s":
            self.saved = (self.cx, self.cy)
        elif final == "u":
            self.cx, self.cy = self.saved
        elif final == "S":
            self._scroll_up(n)
        elif final == "T":
            self._scroll_down(n)
        elif final == "L":
            for _ in range(n):
                if self.top <= self.cy <= self.bottom:
                    del self.grid[self.bottom]
                    self.grid.insert(self.cy, [" "] * self.cols)
        elif final == "M":
            for _ in range(n):
                if self.top <= self.cy <= self.bottom:
                    del self.grid[self.cy]
                    self.grid.insert(self.bottom, [" "] * self.cols)

    def _erase_line(self, y, start, end):
        for x in range(start, min(end, self.cols)):
            self.grid[y][x] = " "


def make_stream_file(kind, count, directory):
    path = os.path.join(directory, "stream.txt")
    with open(path, "w") as handle:
        if kind == "tokens":
            handle.write("".join(f"T{i:05d} " for i in range(count)))
        else:
            for i in range(count):
                handle.write(f"L{i:05d} the quick brown fox jumps over the lazy dog\n")
    return path


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * fraction))]


def read_timeline(path):
    marks = {}
    chunks = {}
    if not os.path.exists(path):
        return marks, chunks
    for line in open(path):
        label, micros = line.split()
        seconds = int(micros) / 1e6
        if label.startswith("chunk"):
            chunks[int(label[5:])] = seconds
        else:
            marks.setdefault(label, seconds)
    return marks, chunks


def main():
    cmd = sys.argv[1:]
    secs = float(os.environ.get("PROBE_SECS", "6"))
    prompt = os.environ.get("PROBE_PROMPT")
    type_at = float(os.environ.get("PROBE_TYPE_AT", "2.0"))
    rows = int(os.environ.get("PROBE_ROWS", "40"))
    cols = int(os.environ.get("PROBE_COLS", "120"))
    verbose = os.environ.get("PROBE_VERBOSE") == "1"
    ctrl_l_at = os.environ.get("PROBE_CTRL_L_AT")
    ctrl_l_at = float(ctrl_l_at) if ctrl_l_at else None
    stream = os.environ.get("PROBE_STREAM")
    chunk_bytes = int(os.environ.get("PROBE_STREAM_CHUNK", "8"))
    scratch = tempfile.mkdtemp(prefix="pty_probe_")
    timeline_path = os.path.join(scratch, "timeline.txt")
    os.environ["MOCK_AGENT_TIMELINE_FILE"] = timeline_path
    frame_trace_path = os.path.join(scratch, "frames.txt")
    os.environ["NORI_FRAME_TRACE"] = frame_trace_path
    stream_count = 0
    if stream:
        stream_count = int(
            os.environ.get("PROBE_STREAM_COUNT", "400" if stream == "tokens" else "2000")
        )
        os.environ["MOCK_AGENT_STREAM_TEXT_FILE"] = make_stream_file(
            stream, stream_count, scratch
        )
        os.environ["MOCK_AGENT_STREAM_CHUNK_BYTES"] = str(chunk_bytes)
        os.environ["MOCK_AGENT_STREAM_DELAY_MS"] = os.environ.get(
            "PROBE_STREAM_DELAY_MS", "0"
        )

    exec_epoch = time.time()
    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(cmd[0], cmd)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    t0 = exec_epoch
    screen = Screen(rows, cols)
    found = {}  # stream id -> first time it was on the screen
    scanned = 0
    reads = []  # (epoch, nbytes, raw)
    visible_text = ""
    visible_ends = []  # (offset_end_in_visible_text, epoch)
    first = glyph = entered = ctrl_l_sent = None
    echo = None
    typed = False
    enter_due = 0.0
    sync_open = False
    frames = []  # reads per synchronized frame
    frame_reads = 0
    frame_start_times = []
    unframed_after_enter = 0
    ctrl_l_first = ctrl_l_done = None
    screen_before_ctrl_l = ctrl_l_screen = None
    ctrl_l_bytes = b""
    while time.time() - t0 < secs:
        now = time.time() - t0
        if prompt and not typed and glyph is not None and now >= type_at:
            os.write(fd, prompt.encode())
            typed = True
            enter_due = time.time() + 0.15  # keep reading while the typed text settles
        if typed and entered is None and time.time() >= enter_due:
            entered = time.time()
            os.write(fd, b"\r")
            print(f"{(entered - t0) * 1000:8.1f}  ENTER")
        if (
            ctrl_l_at is not None
            and entered is not None
            and ctrl_l_sent is None
            and time.time() - entered >= ctrl_l_at
        ):
            ctrl_l_sent = time.time()
            screen_before_ctrl_l = (screen.screen_text(), list(screen.scrollback))
            os.write(fd, b"\x0c")
            print(f"{(ctrl_l_sent - t0) * 1000:8.1f}  CTRL+L")
        ready, _, _ = select.select([fd], [], [], 0.002)
        if not ready:
            continue
        try:
            data = os.read(fd, 1 << 16)
        except OSError:
            break
        stamp = time.time()
        now = stamp - t0
        reads.append((stamp, len(data), data))
        for query, answer, name in QUERIES:
            for _ in range(data.count(query)):
                os.write(fd, answer)
                print(f"{now * 1000:8.1f}  query {name}")
        screen.feed(data)
        fresh = screen.scrollback[scanned:] + screen.screen_text()
        scanned = len(screen.scrollback)
        for row in fresh:
            for match in STREAM_ID.finditer(row):
                found.setdefault(match.group(1), stamp)
        stripped = ANSI.sub(b"", data).replace(b"\r", b"")
        text = stripped.decode("utf-8", "replace")
        visible_text += text
        visible_ends.append((len(visible_text), stamp))
        if first is None and stripped.strip():
            first = now
            print(f"{now * 1000:8.1f}  first visible bytes")
        if glyph is None and "›" in visible_text:
            glyph = now
            print(f"{now * 1000:8.1f}  composer glyph")
        if entered is not None and echo is None and prompt and prompt in text:
            echo = stamp - entered
            print(f"{now * 1000:8.1f}  prompt echoed ({echo * 1000:.1f} ms after Enter)")
        if ctrl_l_sent is not None and ctrl_l_done is None:
            if ctrl_l_first is None:
                ctrl_l_first = stamp - ctrl_l_sent
            ctrl_l_bytes += data
            if SYNC_END in data:
                ctrl_l_done = stamp - ctrl_l_sent
                ctrl_l_screen = (screen.screen_text(), list(screen.scrollback))
        # Frame accounting: count pty reads between sync begin and sync end.
        position = 0
        while position < len(data):
            if not sync_open:
                begin = data.find(SYNC_BEGIN, position)
                if begin < 0:
                    if entered is not None and data[position:].strip():
                        unframed_after_enter += 1
                    break
                sync_open = True
                frame_reads = 0
                frame_start_times.append(stamp)
                position = begin + len(SYNC_BEGIN)
                frame_reads += 1
            else:
                end = data.find(SYNC_END, position)
                if end < 0:
                    frame_reads += 1
                    break
                sync_open = False
                frames.append(frame_reads)
                position = end + len(SYNC_END)
        if entered is not None and verbose:
            text_line = " ".join(text.split())[:70]
            print(f"   +{(stamp - entered) * 1000:7.1f}  {len(data):6d}B  {text_line}")

    print(f"total bytes {sum(r[1] for r in reads)}")
    dump = os.environ.get("PROBE_DUMP")
    if dump:
        with open(dump, "wb") as handle:
            handle.write(b"".join(r[2] for r in reads))
    os.kill(pid, signal.SIGKILL)

    marks, chunks = read_timeline(timeline_path)
    if os.environ.get("PROBE_SHOW_SCREEN") == "1":
        print("--- final screen ---")
        for row in screen.screen_text():
            print(row)
        print(f"scrollback rows: {len(screen.scrollback)}")
    print("--- summary (ms) ---")
    if first is not None:
        print(f"first visible bytes after exec : {first * 1000:8.1f}")
    if glyph is not None:
        print(f"composer glyph after exec      : {glyph * 1000:8.1f}")
    if "process_start" in marks:
        print(f"agent process started after exec: {(marks['process_start'] - exec_epoch) * 1000:8.1f}")
    if "new_session" in marks:
        print(f"agent session/new after exec   : {(marks['new_session'] - exec_epoch) * 1000:8.1f}")
    if entered is not None and "prompt" in marks:
        print(f"Enter -> agent saw session/prompt: {(marks['prompt'] - entered) * 1000:8.1f}")
    if echo is not None:
        print(f"Enter -> user cell painted     : {echo * 1000:8.1f}")
    elif entered is not None:
        print("Enter -> user cell painted     : never")
    if frames:
        histogram = {}
        for count in frames:
            histogram[count] = histogram.get(count, 0) + 1
        print(
            f"sync frames {len(frames)}, reads/frame max {max(frames)} "
            f"mean {sum(frames) / len(frames):.2f}, histogram {dict(sorted(histogram.items()))}, "
            f"unframed reads after Enter {unframed_after_enter}"
        )
    if os.path.exists(frame_trace_path):
        commits = [line.split() for line in open(frame_trace_path)]
        if commits:
            calls = [int(c[2]) for c in commits]
            sizes = [int(c[1]) for c in commits]
            histogram = {}
            for count in calls:
                histogram[count] = histogram.get(count, 0) + 1
            print(
                f"frame commits {len(commits)}: write(2) calls per frame max {max(calls)}, "
                f"histogram {dict(sorted(histogram.items()))}; frame bytes max {max(sizes)} "
                f"mean {sum(sizes) // len(sizes)}"
            )
    if stream == "tokens" and chunks:
        latencies = []
        missing = 0
        for index in range(stream_count):
            painted = found.get(f"T{index:05d}")
            sent_chunk = (index * TOKEN_WIDTH + 5) // chunk_bytes
            if painted is None:
                missing += 1
            elif sent_chunk in chunks:
                latencies.append((painted - chunks[sent_chunk]) * 1000)
        if latencies:
            print(
                f"delta -> paint ({len(latencies)} tokens, {missing} never on screen): "
                f"p50 {percentile(latencies, 0.5):.1f} p95 {percentile(latencies, 0.95):.1f} "
                f"max {max(latencies):.1f}"
            )
    if stream == "lines" and chunks:
        last_label = f"L{stream_count - 1:05d}"
        painted = found.get(last_label)
        last_sent = chunks[max(chunks)]
        first_sent = chunks[min(chunks)]
        seen = sum(1 for index in range(stream_count) if f"L{index:05d}" in found)
        if painted is not None:
            print(
                f"burst: agent sent {len(chunks)} chunks over {(last_sent - first_sent) * 1000:.0f} ms; "
                f"last line on screen {(painted - last_sent) * 1000:.1f} ms after the last chunk was sent; "
                f"{seen}/{stream_count} lines seen"
            )
        else:
            print(f"burst: last line {last_label} never on screen ({seen}/{stream_count} seen)")
    if ctrl_l_sent is not None:
        print(
            "Ctrl+L -> first byte "
            + (f"{ctrl_l_first * 1000:.1f}" if ctrl_l_first is not None else "never")
            + " ms, frame end "
            + (f"{ctrl_l_done * 1000:.1f}" if ctrl_l_done is not None else "never")
            + " ms; emitted 2J="
            + str(b"\x1b[2J" in ctrl_l_bytes)
            + " 3J="
            + str(b"\x1b[3J" in ctrl_l_bytes)
            + f" bytes={len(ctrl_l_bytes)}"
        )
        if screen_before_ctrl_l and ctrl_l_screen:
            same_screen = screen_before_ctrl_l[0] == ctrl_l_screen[0]
            same_scrollback = screen_before_ctrl_l[1] == ctrl_l_screen[1]
            print(f"Ctrl+L left the screen identical: {same_screen}; scrollback identical: {same_scrollback}")
            if not same_screen:
                for row, (a, b) in enumerate(zip(screen_before_ctrl_l[0], ctrl_l_screen[0])):
                    if a != b:
                        print(f"  row {row}: {a!r} -> {b!r}")


main()
