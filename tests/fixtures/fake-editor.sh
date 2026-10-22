#!/bin/sh
# A stand-in editor for the tests: `fake-editor.sh FILE`.
#
#   FAKE_EDITOR_RECORD   prefix for what the editor saw: `<prefix>.count` (one
#                        line per call), `<prefix>.ls.N` (`ls -l` of FILE),
#                        `<prefix>.in.N` (a copy of the buffer as opened) and
#                        `<prefix>.link.N` (a hard link to FILE, so the test can
#                        inspect it after anvil scrubbed and removed it)
#   FAKE_EDITOR_EXIT     exit with this status without touching FILE
#   FAKE_EDITOR_CONTENT  copy this file over FILE (`<content>.N` wins when it
#                        exists, N being the call number)
set -eu

file=$1
n=1
if [ -n "${FAKE_EDITOR_RECORD:-}" ]; then
	if [ -f "$FAKE_EDITOR_RECORD.count" ]; then
		n=$(($(wc -l <"$FAKE_EDITOR_RECORD.count") + 1))
	fi
	echo "$n" >>"$FAKE_EDITOR_RECORD.count"
	ls -l "$file" >"$FAKE_EDITOR_RECORD.ls.$n"
	cp "$file" "$FAKE_EDITOR_RECORD.in.$n"
	ln "$file" "$FAKE_EDITOR_RECORD.link.$n"
fi

if [ -n "${FAKE_EDITOR_EXIT:-}" ]; then
	exit "$FAKE_EDITOR_EXIT"
fi

if [ -n "${FAKE_EDITOR_CONTENT:-}" ]; then
	if [ -f "$FAKE_EDITOR_CONTENT.$n" ]; then
		cp "$FAKE_EDITOR_CONTENT.$n" "$file"
	else
		cp "$FAKE_EDITOR_CONTENT" "$file"
	fi
fi
