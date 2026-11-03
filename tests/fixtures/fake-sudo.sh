#!/bin/sh
# Stands in for sudo in the wrapper tests: logs its arguments, runs nothing.
printf '%s\n' "$*" >>"$FAKE_SUDO_LOG"
