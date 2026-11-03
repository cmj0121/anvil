#!/bin/sh
# Stands in for ps in the wrapper tests: prints FAKE_PS_OUT whatever it is asked.
printf '%s\n' "$FAKE_PS_OUT"
