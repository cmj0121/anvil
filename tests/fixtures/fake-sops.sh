#!/bin/sh
# A stand-in sops for the unit tests. anvil clears sops' environment, so the
# knobs come from `<root>/fake-sops.env`, sourced when present, for a store
# at `<root>/<dir>/<file>`:
#
#   FAKE_SOPS_FAIL    `decrypt` or `encrypt`: that subcommand prints
#                     FAKE_SOPS_STDERR and exits with FAKE_SOPS_EXIT (128)
#   FAKE_SOPS_SLEEP   seconds to sleep first
#   FAKE_SOPS_PAD     bytes of spaces `encrypt` appends to its output
#
# What sops saw goes to `<root>/record.argv` (each argument on its own line,
# then a blank line, per call), `<root>/record.env` (the environment),
# `<root>/record.cwd` and `<root>/record.ls` (the store directory's entries
# while encrypting).
#
# The "ciphertext" is the plaintext with letters and digits rotated (rot13,
# rot5), so it never holds a test value verbatim.
set -eu

subcommand=
for arg in "$@"; do
	case "$arg" in
	decrypt | encrypt) [ -z "$subcommand" ] && subcommand=$arg ;;
	esac
	store=$arg
done
root=$(dirname "$(dirname "$store")")
if [ -f "$root/fake-sops.env" ]; then
	. "$root/fake-sops.env"
fi

rec=$root/record
for arg in "$@"; do
	printf '%s\n' "$arg"
done >>"$rec.argv"
echo >>"$rec.argv"
env >"$rec.env"
pwd >"$rec.cwd"

if [ -n "${FAKE_SOPS_SLEEP:-}" ]; then
	sleep "$FAKE_SOPS_SLEEP"
fi
if [ "$subcommand" = "${FAKE_SOPS_FAIL:-}" ]; then
	printf '%s\n' "${FAKE_SOPS_STDERR:-}" >&2
	exit "${FAKE_SOPS_EXIT:-128}"
fi

rotate() {
	tr 'A-Za-z0-9' 'N-ZA-Mn-za-m5-90-4'
}

case "$subcommand" in
decrypt)
	rotate <"$store"
	;;
encrypt)
	ls -a "$(dirname "$store")" >"$rec.ls"
	rotate
	if [ -n "${FAKE_SOPS_PAD:-}" ]; then
		head -c "$FAKE_SOPS_PAD" /dev/zero | tr '\0' ' '
	fi
	;;
*)
	echo "fake-sops: unexpected arguments" >&2
	exit 2
	;;
esac
