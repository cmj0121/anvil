#!/bin/sh
# Fake ssh for the tests. Driven by env vars set on the child, or sourced from
# `<home>/fake-ssh.env` when present, for a ControlPath of `<home>/.ssh/anvil/%C`:
#   FAKE_SSH_LOG            append each invocation's argv (space-joined) as one line
#   FAKE_SSH_CHECK_EXIT     exit code of `-O check` (default 1: no master)
#   FAKE_SSH_EXIT           exit code of the key (BatchMode) master attempt (default 0)
#   FAKE_SSH_CHECK_SLEEP    seconds `-O check` sleeps first
#   FAKE_SSH_SLEEP          seconds the key master attempt sleeps first
#   FAKE_SSH_STDERR         stderr text of the key master attempt
#   FAKE_SSH_STDERR_BYTES   bytes of filler the key master attempt writes to stderr
#   FAKE_SSH_RETRY_EXIT     exit code of the interactive master attempt (default 0)
#   FAKE_SSH_RETRY_STDERR   stderr text of the interactive master attempt
#   FAKE_SSH_CLIENT_EXIT    exit code of the client; unset runs the script with sh -s
#   FAKE_SSH_CLIENT_STDERR  stderr text of the client when FAKE_SSH_CLIENT_EXIT is set
# A client with -tt runs its last argument (the remote command) under script(1),
# so it gets a terminal as on a real host.
for arg in "$@"; do
  case "$arg" in
    ControlPath=*/.ssh/anvil/*)
      control=${arg#ControlPath=}
      knobs=${control%/.ssh/anvil/*}/fake-ssh.env
      if [ -f "$knobs" ]; then
        . "$knobs"
      fi
      ;;
  esac
done

if [ -n "${FAKE_SSH_LOG-}" ]; then
  printf '%s\n' "$*" >>"$FAKE_SSH_LOG"
fi

kind=client
batch=no
tty=no
for arg in "$@"; do
  case "$arg" in
    -tt) tty=yes ;;
    check) [ "$prev" = "-O" ] && kind=check ;;
    -M) kind=master ;;
    BatchMode=yes) batch=yes ;;
  esac
  prev=$arg
done

case "$kind" in
  check)
    [ -n "${FAKE_SSH_CHECK_SLEEP-}" ] && sleep "$FAKE_SSH_CHECK_SLEEP"
    exit "${FAKE_SSH_CHECK_EXIT-1}"
    ;;
  master)
    if [ "$batch" = yes ]; then
      [ -n "${FAKE_SSH_SLEEP-}" ] && sleep "$FAKE_SSH_SLEEP"
      [ -n "${FAKE_SSH_STDERR_BYTES-}" ] && yes | head -c "$FAKE_SSH_STDERR_BYTES" >&2
      [ -n "${FAKE_SSH_STDERR-}" ] && printf '%s\n' "$FAKE_SSH_STDERR" >&2
      exit "${FAKE_SSH_EXIT-0}"
    fi
    [ -n "${FAKE_SSH_RETRY_STDERR-}" ] && printf '%s\n' "$FAKE_SSH_RETRY_STDERR" >&2
    exit "${FAKE_SSH_RETRY_EXIT-0}"
    ;;
  client)
    if [ -n "${FAKE_SSH_CLIENT_EXIT-}" ]; then
      [ -n "${FAKE_SSH_CLIENT_STDERR-}" ] && printf '%s\n' "$FAKE_SSH_CLIENT_STDERR" >&2
      exit "$FAKE_SSH_CLIENT_EXIT"
    fi
    if [ "$tty" = yes ]; then
      for remote in "$@"; do :; done
      export SHELL=/bin/sh
      script=/usr/bin/script
      [ -e "$script" ] || script=/bin/script
      case "$(uname)" in
        Linux) exec "$script" -qec "$remote" /dev/null ;;
        *) exec "$script" -q /dev/null /bin/sh -c "$remote" ;;
      esac
    fi
    exec sh -s
    ;;
esac
