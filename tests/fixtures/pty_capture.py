#!/usr/bin/env python3
"""Run a command with stdout and stderr on one pseudo-terminal, keep its bytes.

    pty_capture.py -- COMMAND [ARG...]

The command's raw output (escape sequences and `\\r` kept, as the terminal
received it) goes to stdout; exits with the command's exit code, or 124
after a timeout. Standard library only; needs os.login_tty.
"""

import os
import select
import sys
import time

TIMEOUT = 30


def main() -> int:
    if len(sys.argv) < 3 or sys.argv[1] != "--":
        print(__doc__, file=sys.stderr)
        return 2
    command = sys.argv[2:]
    master, slave = os.openpty()
    pid = os.fork()
    if pid == 0:
        os.close(master)
        os.login_tty(slave)
        os.execvp(command[0], command)
    os.close(slave)

    transcript = b""
    deadline = time.monotonic() + TIMEOUT
    while True:
        left = deadline - time.monotonic()
        if left <= 0:
            os.kill(pid, 9)
            os.waitpid(pid, 0)
            sys.stdout.buffer.write(transcript)
            return 124
        ready, _, _ = select.select([master], [], [], min(left, 0.1))
        if not ready:
            continue
        try:
            chunk = os.read(master, 4096)
        except OSError:
            # EIO: every slave fd closed, the command is gone.
            break
        if not chunk:
            break
        transcript += chunk
    os.close(master)
    _, status = os.waitpid(pid, 0)
    sys.stdout.buffer.write(transcript)
    return os.waitstatus_to_exitcode(status)


if __name__ == "__main__":
    sys.exit(main())
