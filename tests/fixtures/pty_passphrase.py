#!/usr/bin/env python3
"""Run a command on a pseudo-terminal and answer its passphrase prompt.

    pty_passphrase.py --reply {passphrase,ctrl-c} --prompts N --exit N -- COMMAND [ARG...]

The passphrase comes from $PTY_PASSPHRASE, removed from the environment before
the command starts, and is typed only when a line ends in `passphrase for
...: `; it never appears on argv. The terminal's settings are read while the
prompt waits (echo, line editing and signals must be off) and again after the
command exits (all three must be back on). Passes (exit 0) when the command
exits N, was prompted N times and the passphrase never shows in its output.
The transcript goes to stdout. Standard library only.
"""

import argparse
import json
import os
import re
import select
import signal
import sys
import termios
import time

TIMEOUT = 30
PROMPT = re.compile(rb"passphrase for [^\n]*: $")
FLAGS = {"echo": termios.ECHO, "icanon": termios.ICANON, "isig": termios.ISIG}
AFTER = re.compile(r"^pty_passphrase: after (\{.*\})$", re.MULTILINE)


def modes(fd: int) -> dict:
    lflag = termios.tcgetattr(fd)[3]
    return {name: bool(lflag & bit) for name, bit in FLAGS.items()}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--reply", choices=["passphrase", "ctrl-c"], required=True)
    parser.add_argument("--prompts", type=int, required=True)
    parser.add_argument("--exit", type=int, required=True)
    parser.add_argument("command", nargs="+")
    args = parser.parse_args()
    # Popped, so the passphrase never reaches the command's environment.
    passphrase = os.environ.pop("PTY_PASSPHRASE", "").encode()

    # The session leader outlives the command, so the terminal is still its
    # own when it reads the settings the command left behind.
    master, slave = os.openpty()
    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.login_tty(slave)
        child = os.fork()
        if child == 0:
            os.execvp(args.command[0], args.command)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        _, status = os.waitpid(child, 0)
        print(f"pty_passphrase: after {json.dumps(modes(0))}", flush=True)
        code = os.waitstatus_to_exitcode(status)
        os._exit(code if code >= 0 else 128 - code)

    transcript = b""
    pending = b""
    prompts = 0
    during = None
    deadline = time.monotonic() + TIMEOUT
    timed_out = False
    exited = None
    while exited is None:
        left = deadline - time.monotonic()
        if left <= 0:
            timed_out = True
            os.kill(pid, 9)
            break
        ready, _, _ = select.select([master], [], [], min(left, 0.1))
        if ready:
            try:
                chunk = os.read(master, 4096)
            except OSError:
                chunk = b""
            if not chunk:
                break
            transcript += chunk
            pending += chunk
            if PROMPT.search(pending):
                prompts += 1
                pending = b""
                during = modes(slave)
                reply = passphrase + b"\r" if args.reply == "passphrase" else b"\x03"
                os.write(master, reply)
            continue
        done, status = os.waitpid(pid, os.WNOHANG)
        if done:
            exited = os.waitstatus_to_exitcode(status)
    if exited is None:
        _, status = os.waitpid(pid, 0)
        exited = os.waitstatus_to_exitcode(status)
    # The last output can still be in flight when the exit is seen.
    while select.select([master], [], [], 0.2)[0]:
        try:
            chunk = os.read(master, 4096)
        except OSError:
            break
        if not chunk:
            break
        transcript += chunk
    os.close(master)
    os.close(slave)

    text = transcript.decode(errors="replace").replace("\r\n", "\n")
    found = AFTER.search(text)
    after = json.loads(found.group(1)) if found else {}
    sys.stdout.write(text)
    problems = []
    if timed_out:
        problems.append(f"timed out after {TIMEOUT} s")
    if exited != args.exit:
        problems.append(f"exit {exited}, expected {args.exit}")
    if prompts != args.prompts:
        problems.append(f"prompted {prompts} times, expected {args.prompts}")
    if passphrase and passphrase in transcript:
        problems.append("the passphrase shows in the output")
    if during is not None and any(during.values()):
        problems.append(f"while prompting {during}, expected all off")
    if not after or not all(after.values()):
        problems.append(f"after exit {after}, expected all on")
    print(f"pty_passphrase: prompts={prompts} exit={exited} during={during} after={after}")
    for problem in problems:
        print(f"pty_passphrase: {problem}")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
