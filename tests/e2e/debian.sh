#!/bin/sh
# The `apply --locked` e2e on a Debian 12 sshd container (Dockerfile.debian):
# tzdata is put at an older version the mirror still offers (the current one
# when it offers only one), `anvil lock` records it, tzdata is moved off it,
# then `plan --locked` shows the versioned install and `apply --locked`
# brings tzdata back to the locked version, which dpkg then reports. A
# version the mirror does not offer fails the target with the D7 message.
# Run alone (`sh tests/e2e/debian.sh`) or from tests/e2e.sh, which passes the
# built binary as ANVIL. Needs docker and the Debian mirror; prints `skip`
# and passes without docker. Never touches the real ~/.ssh or a real host.
set -u

cd "$(dirname "$0")/../.." || exit 1

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo skip
  exit 0
fi

IMAGE=anvil-e2e-debian
NAME="anvil-e2e-debian-$$"
TMP=$(mktemp -d /tmp/anvil-e2e-deb.XXXXXX) || exit 1
XDG="$TMP/xdg"
CFG="$TMP/ssh_config"
KEY="$TMP/id_ed25519"
LOCK="$XDG/anvil/anvil.lock"
fails=0

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

ok() { printf 'ok    debian: %s\n' "$*"; }
bad() {
  printf 'FAIL  debian: %s\n' "$*"
  fails=$((fails + 1))
  cat "$TMP/out" "$TMP/err" 2>/dev/null
}

docker build -q -f tests/e2e/Dockerfile.debian -t "$IMAGE" tests/e2e >/dev/null || exit 1
if [ -z "${ANVIL:-}" ]; then
  cargo build --quiet || exit 1
  ANVIL=$(pwd)/target/debug/anvil
fi

ssh-keygen -q -t ed25519 -N '' -f "$KEY" || exit 1
docker run -d --rm --name "$NAME" --label anvil-e2e=1 --label "anvil-e2e.pid=$$" \
  -p 127.0.0.1::22 -e AUTHORIZED_KEY="$(cat "$KEY.pub")" "$IMAGE" >/dev/null || exit 1
PORT=$(docker port "$NAME" 22/tcp | head -n1 | sed 's/.*://')
[ -n "$PORT" ] || { echo "no mapped port"; exit 1; }
i=0
until ssh-keyscan -T 2 -p "$PORT" 127.0.0.1 2>/dev/null | grep ssh-ed25519 >"$TMP/known_hosts"; do
  i=$((i + 1))
  [ "$i" -lt 60 ] || { echo "sshd did not come up"; docker logs "$NAME"; exit 1; }
  sleep 1
done

cat >"$CFG" <<CONF
Host *
  UserKnownHostsFile $TMP/known_hosts
  GlobalKnownHostsFile /dev/null
  IdentityFile $KEY
  IdentitiesOnly yes
  PubkeyAuthentication yes
CONF
mkdir -p "$XDG/anvil" "$TMP/home"
printf '[hosts.deb]\nhost = "127.0.0.1"\nuser = "key"\nport = %s\n' "$PORT" >"$XDG/anvil/inventory.toml"
printf '[packages]\ninstall = ["tzdata"]\n' >"$XDG/anvil/config.toml"

anvil() {
  env -u VISUAL HOME="$TMP/home" XDG_CONFIG_HOME="$XDG" XDG_STATE_HOME="$TMP/state" \
    "$ANVIL" --ssh-config "$CFG" "$@" </dev/null >"$TMP/out" 2>"$TMP/err"
}
on_box() { docker exec -e DEBIAN_FRONTEND=noninteractive "$NAME" "$@" >"$TMP/box" 2>&1; }
installed() { docker exec "$NAME" dpkg-query -W -f='${Version}' tzdata 2>/dev/null; }

on_box apt-get update -q || { cat "$TMP/box"; exit 1; }
CANDIDATE=$(docker exec "$NAME" apt-cache policy tzdata | awk '/Candidate:/ {print $2}')
OLD=$(docker exec "$NAME" apt-cache madison tzdata | awk -v c="$CANDIDATE" '$3 != c {print $3; exit}')
[ -n "$OLD" ] || OLD=$CANDIDATE
echo "debian: tzdata candidate $CANDIDATE, locking $OLD"

on_box apt-get install -y --allow-downgrades "tzdata=$OLD" || { cat "$TMP/box"; exit 1; }
anvil lock --target deb
code=$?
if [ "$code" -eq 0 ] && grep -qxF "tzdata = \"$OLD\"" "$LOCK" \
  && grep -q '^manager = "apt"$' "$LOCK" && grep -q '^distro = "debian"$' "$LOCK"; then
  ok "anvil lock records tzdata $OLD"
else
  bad "anvil lock: exit $code"
  cat "$LOCK" 2>/dev/null
fi

if [ "$OLD" != "$CANDIDATE" ]; then
  on_box apt-get install -y tzdata
else
  on_box dpkg --purge --force-depends tzdata
fi
moved=$(installed)
anvil -vv plan --locked --target deb
code=$?
# shellcheck disable=SC2016 # $b is the script's own variable, matched as text
if [ "$code" -eq 0 ] && grep -qF "'tzdata=$OLD'" "$TMP/out" \
  && grep -qF 'apt-get install -y --allow-downgrades $b' "$TMP/out"; then
  ok "plan --locked shows the versioned install"
else
  bad "plan --locked: exit $code"
fi

anvil apply --locked --target deb
code=$?
now=$(installed)
if [ "$code" -eq 0 ] && [ "$now" = "$OLD" ]; then
  ok "apply --locked: tzdata ${moved:-not installed} -> $now"
else
  bad "apply --locked: exit $code, tzdata ${now:-not installed}, want $OLD"
fi

anvil apply --locked --target deb
code=$?
if [ "$code" -eq 0 ] && [ "$(installed)" = "$OLD" ]; then
  ok "a second apply --locked passes"
else
  bad "second apply --locked: exit $code"
fi

sed "s/^tzdata = .*/tzdata = \"0.0+anvil.gone\"/" "$LOCK" >"$TMP/lock" && cat "$TMP/lock" >"$LOCK"
anvil apply --locked --target deb
code=$?
if [ "$code" -eq 1 ] && grep -qF 'packages: tzdata 0.0+anvil.gone is no longer in the repositories' "$TMP/out" "$TMP/err" \
  && [ "$(installed)" = "$OLD" ]; then
  ok "a version the mirror lacks fails the target"
else
  bad "gone version: exit $code"
fi

if [ "$fails" -eq 0 ]; then
  echo "debian: all checks passed"
else
  echo "debian: $fails check(s) failed"
  exit 1
fi
