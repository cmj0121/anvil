#!/bin/sh
# Fake ssh for the unit tests (D6). Driven by env vars set on the child:
#   FAKE_SSH_LOG            append each invocation's argv (space-joined) as one line
#   FAKE_SSH_CHECK_EXIT     exit code of `-O check` (default 1: no master)
#   FAKE_SSH_EXIT           exit code of the key (BatchMode) master attempt (default 0)
#   FAKE_SSH_STDERR         stderr text of the key master attempt
#   FAKE_SSH_RETRY_EXIT     exit code of the interactive master attempt (default 0)
#   FAKE_SSH_RETRY_STDERR   stderr text of the interactive master attempt
#   FAKE_SSH_CLIENT_EXIT    exit code of the client; unset runs the script with sh -s
#   FAKE_SSH_CLIENT_STDERR  stderr text of the client when FAKE_SSH_CLIENT_EXIT is set
if [ -n "${FAKE_SSH_LOG-}" ]; then
  printf '%s\n' "$*" >>"$FAKE_SSH_LOG"
fi

kind=client
batch=no
for arg in "$@"; do
  case "$arg" in
    check) [ "$prev" = "-O" ] && kind=check ;;
    -M) kind=master ;;
    BatchMode=yes) batch=yes ;;
  esac
  prev=$arg
done

case "$kind" in
  check)
    exit "${FAKE_SSH_CHECK_EXIT-1}"
    ;;
  master)
    if [ "$batch" = yes ]; then
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
    exec sh -s
    ;;
esac
