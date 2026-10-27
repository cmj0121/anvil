#!/bin/sh
# A stand-in plugin for the tests. anvil clears a plugin's environment and
# runs it in its own directory, so the knobs come from `./fake-plugin.env`,
# sourced when present:
#
#   FAKE_PLUGIN_REPLY   what it prints on stdout ({"script":"hostname"})
#   FAKE_PLUGIN_SCRIPT  instead, a reply whose script is this many bytes of x
#   FAKE_PLUGIN_PAD     bytes of spaces printed after the reply
#   FAKE_PLUGIN_STDERR  printed on stderr
#   FAKE_PLUGIN_SLEEP   seconds to sleep before replying
#   FAKE_PLUGIN_EXIT    its exit status (0)
#   FAKE_PLUGIN_KILL    when set, it kills itself with SIGKILL instead
#   FAKE_PLUGIN_DETACH  when set, leaves a 3s sleep in the background, out of
#                       its process group (perl's `setpgrp`), holding stdout
#                       and stderr open
#   FAKE_PLUGIN_HOLD_STDIN  when set, replies without reading stdin and leaves
#                       `sleep 30` in the background holding it, its pid in
#                       `./record.holder`
#
# What it saw goes to `./record.stdin`, `./record.env`, `./record.argc` and
# `./record.cwd`.
set -eu

if [ -f fake-plugin.env ]; then
	. ./fake-plugin.env
fi

if [ -n "${FAKE_PLUGIN_HOLD_STDIN:-}" ]; then
	sleep 30 <&0 >/dev/null 2>&1 &
	echo "$!" >record.holder
	printf '%s\n' '{"script":"hostname"}'
	exit 0
fi

cat >record.stdin
env >record.env
echo "$#" >record.argc
pwd >record.cwd

if [ -n "${FAKE_PLUGIN_SLEEP:-}" ]; then
	sleep "$FAKE_PLUGIN_SLEEP"
fi
if [ -n "${FAKE_PLUGIN_STDERR:-}" ]; then
	printf '%s\n' "$FAKE_PLUGIN_STDERR" >&2
fi
if [ -n "${FAKE_PLUGIN_DETACH:-}" ]; then
	perl -e 'setpgrp; sleep 3' &
	# Out of the group before this exits and the group is killed.
	while [ "$(ps -o pgid= -p "$!" | tr -d ' ')" = "$$" ]; do
		sleep 0.05
	done
fi
if [ -n "${FAKE_PLUGIN_KILL:-}" ]; then
	kill -KILL $$
fi
if [ -n "${FAKE_PLUGIN_SCRIPT:-}" ]; then
	printf '{"script":"'
	head -c "$FAKE_PLUGIN_SCRIPT" /dev/zero | tr '\0' x
	printf '"}\n'
else
	reply=${FAKE_PLUGIN_REPLY:-}
	[ -n "$reply" ] || reply='{"script":"hostname"}'
	printf '%s\n' "$reply"
fi
if [ -n "${FAKE_PLUGIN_PAD:-}" ]; then
	head -c "$FAKE_PLUGIN_PAD" /dev/zero | tr '\0' ' '
fi
exit "${FAKE_PLUGIN_EXIT:-0}"
