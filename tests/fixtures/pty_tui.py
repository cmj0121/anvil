#!/usr/bin/env python3
"""Drive a full-screen program on a pseudo-terminal, step by step.

    pty_tui.py [--rows R] [--cols C] [--prompts N] [--exit N] [--entries N]
               [--main REGEX] (--wait REGEX | --send KEYS)... -- COMMAND [ARG...]

The steps run in order: `--wait` reads until the screen the program shows
matches REGEX (Python syntax, searched over its rows joined by newlines),
`--send` types KEYS (backslash escapes such as `\\r` and `\\x1b` allowed).
The screen comes from a small terminal model: cursor moves, printing, the
alternate screen and erases; colours and other sequences are dropped.

With $PTY_PASSWORD set (removed from the environment before the command
starts), the password is typed whenever the output ends in `password:`, at
any step; it never appears on argv. `--prompts N` requires exactly N such
prompts. Passes (exit 0) when every wait matched within the timeout, the
command exited N (default 0), it entered and left the alternate screen at
least `--entries` times (default 1), left it after it last entered it and
showed the cursor again after that. `--main` must match the main screen at
the end: what the command printed while it was off the alternate screen.
The last alternate screen and the checks go to stdout. Standard library
only; needs Python 3.11 or newer (os.login_tty).
"""

import argparse
import codecs
import fcntl
import os
import re
import select
import signal
import struct
import sys
import termios
import time

TIMEOUT = 30
PROMPT = re.compile(rb"password:\s*$", re.IGNORECASE)
ENTER_ALT = b"\x1b[?1049h"
LEAVE_ALT = b"\x1b[?1049l"
SHOW_CURSOR = b"\x1b[?25h"


class Screen:
    """The cells of the main and the alternate screen and the cursor."""

    def __init__(self, rows: int, cols: int):
        self.rows, self.cols = rows, cols
        self.main = self.blank()
        self.alt = self.blank()
        self.grid = self.main
        self.row = self.col = 0
        self.saved = (0, 0)
        self.decoder = codecs.getincrementaldecoder("utf-8")("replace")
        self.state = "text"
        self.seq = ""

    def blank(self) -> list:
        return [[" "] * self.cols for _ in range(self.rows)]

    def text(self, grid=None) -> str:
        return "\n".join("".join(row).rstrip() for row in grid or self.grid)

    def feed(self, data: bytes) -> None:
        for ch in self.decoder.decode(data):
            self.char(ch)

    def char(self, ch: str) -> None:
        if self.state == "esc":
            if ch == "[":
                self.state, self.seq = "csi", ""
            elif ch == "]":
                self.state = "osc"
            elif ch in "()":
                self.state = "charset"
            else:
                self.state = "text"
        elif self.state == "charset":
            self.state = "text"
        elif self.state == "osc":
            # BEL ends it; so does ESC \, whose `\` the esc state swallows.
            if ch == "\x07":
                self.state = "text"
            elif ch == "\x1b":
                self.state = "esc"
        elif self.state == "csi":
            if "\x40" <= ch <= "\x7e":
                self.csi(self.seq, ch)
                self.state = "text"
            else:
                self.seq += ch
        elif ch == "\x1b":
            self.state = "esc"
        elif ch == "\r":
            self.col = 0
        elif ch == "\n":
            self.row = min(self.row + 1, self.rows - 1)
        elif ch == "\b":
            self.col = max(self.col - 1, 0)
        elif ch >= " ":
            if self.col < self.cols:
                self.grid[self.row][self.col] = ch
            self.col += 1

    def csi(self, params: str, final: str) -> None:
        if params.startswith("?"):
            # Like xterm: save the cursor and enter a blank alternate
            # screen; on leaving, restore the cursor on the main screen.
            if params == "?1049" and final == "h" and self.grid is self.main:
                self.saved = (self.row, self.col)
                self.alt[:] = self.blank()
                self.grid = self.alt
            elif params == "?1049" and final == "l" and self.grid is self.alt:
                self.row, self.col = self.saved
                self.grid = self.main
            return
        nums = [int(p) if p.isdigit() else 0 for p in params.split(";")]
        first = nums[0] if nums else 0
        if final in "Hf":
            row = nums[0] if nums and nums[0] else 1
            col = nums[1] if len(nums) > 1 and nums[1] else 1
            self.row, self.col = min(row, self.rows) - 1, min(col, self.cols) - 1
        elif final == "J" and first == 2:
            self.grid[:] = self.blank()
        elif final == "K":
            line = self.grid[self.row]
            start = self.col if first == 0 else 0
            end = self.cols if first != 1 else self.col + 1
            line[start:end] = [" "] * (end - start)
        elif final == "A":
            self.row = max(self.row - max(first, 1), 0)
        elif final == "B":
            self.row = min(self.row + max(first, 1), self.rows - 1)
        elif final == "C":
            self.col = min(self.col + max(first, 1), self.cols - 1)
        elif final == "D":
            self.col = max(self.col - max(first, 1), 0)
        elif final == "G":
            self.col = min(max(first, 1), self.cols) - 1


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--rows", type=int, default=30)
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--prompts", type=int)
    parser.add_argument("--exit", type=int, default=0)
    parser.add_argument("--entries", type=int, default=1)
    parser.add_argument("--main")
    parser.add_argument("--wait", dest="steps", action="append", type=lambda s: ("wait", s))
    parser.add_argument("--send", dest="steps", action="append", type=lambda s: ("send", s))
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()
    # Popped, so the password never reaches the command's environment.
    password = os.environ.pop("PTY_PASSWORD", "").encode()

    master, slave = os.openpty()
    size = struct.pack("HHHH", args.rows, args.cols, 0, 0)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, size)
    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.login_tty(slave)
        os.execvp(args.command[0], args.command)
    os.close(slave)

    screen = Screen(args.rows, args.cols)
    transcript = b""
    pending = b""
    prompts = 0
    deadline = time.monotonic() + TIMEOUT
    exited = None
    problems = []

    def pump(wait: float) -> bool:
        """Reads what is there within `wait` seconds; False at end of output."""
        nonlocal transcript, pending, prompts
        ready, _, _ = select.select([master], [], [], max(wait, 0))
        if not ready:
            return True
        try:
            chunk = os.read(master, 65536)
        except OSError:
            chunk = b""
        if not chunk:
            return False
        transcript += chunk
        screen.feed(chunk)
        pending += chunk
        if password and PROMPT.search(pending):
            prompts += 1
            pending = b""
            os.write(master, password + b"\n")
        pending = pending[-256:]
        return True

    open_ = True
    for kind, value in args.steps or []:
        if kind == "send":
            os.write(master, codecs.decode(value, "unicode_escape").encode("latin-1"))
            continue
        pattern = re.compile(value, re.MULTILINE)
        while open_ and not pattern.search(screen.text()):
            left = deadline - time.monotonic()
            if left <= 0:
                break
            open_ = pump(min(left, 0.1))
        if not pattern.search(screen.text()):
            problems.append(f"never saw {value!r}")
            break

    while open_ and time.monotonic() < deadline and not problems:
        open_ = pump(0.1)
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            exited = os.waitstatus_to_exitcode(status)
            break
    if exited is None:
        if open_ or problems:
            os.kill(pid, signal.SIGKILL)
        _, status = os.waitpid(pid, 0)
        exited = os.waitstatus_to_exitcode(status)
    # The last output can still be in flight when the exit is seen.
    while open_ and select.select([master], [], [], 0.2)[0]:
        open_ = pump(0)
    os.close(master)

    sys.stdout.write(screen.text(screen.alt) + "\n")
    if time.monotonic() >= deadline:
        problems.append(f"timed out after {TIMEOUT} s")
    if exited != args.exit:
        problems.append(f"exit {exited}, expected {args.exit}")
    if args.prompts is not None and prompts != args.prompts:
        problems.append(f"prompted {prompts} times, expected {args.prompts}")
    if password and password in transcript:
        problems.append("the password shows in the output")
    if ENTER_ALT not in transcript:
        problems.append("never entered the alternate screen")
    elif transcript.rfind(LEAVE_ALT) < transcript.rfind(ENTER_ALT):
        problems.append("left the terminal on the alternate screen")
    elif transcript.rfind(SHOW_CURSOR) < transcript.rfind(LEAVE_ALT):
        problems.append("left the cursor hidden")
    entries, leaves = transcript.count(ENTER_ALT), transcript.count(LEAVE_ALT)
    if min(entries, leaves) < args.entries:
        problems.append(
            f"entered the alternate screen {entries} times and left it {leaves},"
            f" expected {args.entries} or more"
        )
    if args.main and not re.search(args.main, screen.text(screen.main), re.MULTILINE):
        problems.append(f"the main screen does not match {args.main!r}")
    print(f"pty_tui: prompts={prompts} exit={exited} entries={entries} leaves={leaves}")
    for problem in problems:
        print(f"pty_tui: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
