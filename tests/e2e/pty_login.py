#!/usr/bin/env python3
"""Run a command on a pseudo-terminal and answer ssh's password prompt.

    pty_login.py --prompts N --expect REGEX -- COMMAND [ARG...]

The password comes from $PTY_PASSWORD, removed from the environment before
the command starts, and is written to the terminal only when
a line ends in `password:`; it never appears on argv. Passes (exit 0) when the
command exits 0, its output matches REGEX and it was prompted exactly N times.
The transcript goes to stdout. Standard library only.
"""

import argparse
import os
import pty
import re
import select
import sys
import time

TIMEOUT = 30
PROMPT = re.compile(rb"password:\s*$", re.IGNORECASE)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--prompts", type=int, required=True)
    parser.add_argument("--expect", required=True)
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()
    # Popped, so the password never reaches the command's environment.
    password = os.environ.pop("PTY_PASSWORD", "").encode()

    pid, fd = pty.fork()
    if pid == 0:
        os.execvp(args.command[0], args.command)

    transcript = b""
    pending = b""
    prompts = 0
    deadline = time.monotonic() + TIMEOUT
    timed_out = False
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            timed_out = True
            os.kill(pid, 9)
            break
        ready, _, _ = select.select([fd], [], [], left)
        if not ready:
            continue
        try:
            chunk = os.read(fd, 4096)
        except OSError:
            break
        if not chunk:
            break
        transcript += chunk
        pending += chunk
        if PROMPT.search(pending):
            prompts += 1
            pending = b""
            os.write(fd, password + b"\n")
    _, status = os.waitpid(pid, 0)
    code = os.waitstatus_to_exitcode(status)
    os.close(fd)

    text = transcript.decode(errors="replace").replace("\r\n", "\n")
    sys.stdout.write(text)
    problems = []
    if timed_out:
        problems.append(f"timed out after {TIMEOUT} s")
    if code != 0:
        problems.append(f"exit {code}")
    if prompts != args.prompts:
        problems.append(f"prompted {prompts} times, expected {args.prompts}")
    if not re.search(args.expect, text, re.MULTILINE):
        problems.append(f"output does not match {args.expect!r}")
    print(f"pty_login: prompts={prompts} exit={code}")
    for problem in problems:
        print(f"pty_login: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
