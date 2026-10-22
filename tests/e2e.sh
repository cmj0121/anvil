#!/bin/sh
# End-to-end smoke against an Arch sshd container (D6). U5 covers the ssh
# transport through the hidden `anvil __probe`; U8 extends it to doctor/apply.
# Never touches the real ~/.ssh or a real host. The isolation rests on `-F` with
# a temp ssh config setting UserKnownHostsFile, IdentityFile and IdentitiesOnly
# yes: ssh resolves `~` from the passwd entry, not from $HOME, so the temp HOME
# below only moves anvil's control directory. /etc/ssh/ssh_config still applies.
set -u

cd "$(dirname "$0")/.." || exit 1

if ! command -v docker >/dev/null 2>&1 || ! docker info >/dev/null 2>&1; then
  echo skip
  exit 0
fi

IMAGE=anvil-e2e-sshd
# Base images pinned by manifest-list digest. To bump: `docker buildx imagetools
# inspect archlinux:latest` (and lopsided/archlinux:latest), copy the Digest line.
ARCH_AMD64=archlinux@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0
ARCH_ARM64=lopsided/archlinux@sha256:e895dfb834a8a60b42c2a43d6f447403b94ade083609ba0130032767d2bb34e1
# The official archlinux image is amd64 only and sshd's seccomp sandbox fails
# under emulation, so arm64 hosts build from an Arch Linux ARM image (ID=archarm).
case "$(uname -m)" in
  arm64 | aarch64) BASE=$ARCH_ARM64; PLATFORM=linux/arm64; DISTRO='distro=arch\(arm\)\{0,1\}' ;;
  *) BASE=$ARCH_AMD64; PLATFORM=linux/amd64; DISTRO='distro=arch' ;;
esac
NAME="anvil-e2e-$$"
# Short: the control socket path must fit sun_path (D3), and macOS' $TMPDIR is long.
TMP=$(mktemp -d /tmp/anvil-e2e.XXXXXX) || exit 1
HOME_DIR="$TMP/home"
CFG="$TMP/ssh_config"
KNOWN="$TMP/known_hosts"
EMPTY_KNOWN="$TMP/known_hosts.empty"
KEY="$TMP/id_ed25519"
fails=0

cleanup() {
  docker rm -f "$NAME" >/dev/null 2>&1
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

step() { printf '%s\n' "--- $*"; }
pass() { printf 'ok    %s\n' "$*"; }
fail() { printf 'FAIL  %s\n' "$*"; fails=$((fails + 1)); }

step "build image and binary"
docker build -q --platform "$PLATFORM" --build-arg "BASE=$BASE" -t "$IMAGE" tests/e2e >/dev/null || exit 1
cargo build --quiet || exit 1
ANVIL=target/debug/anvil

step "start sshd"
ssh-keygen -q -t ed25519 -N '' -f "$KEY" || exit 1
docker run -d --rm --platform "$PLATFORM" --name "$NAME" -p 127.0.0.1::22 \
  -e AUTHORIZED_KEY="$(cat "$KEY.pub")" "$IMAGE" >/dev/null || exit 1
PORT=$(docker port "$NAME" 22/tcp | head -n1 | sed 's/.*://')
[ -n "$PORT" ] || { echo "no mapped port"; exit 1; }

i=0
until ssh-keyscan -T 2 -p "$PORT" 127.0.0.1 2>/dev/null | grep -q ssh-ed25519; do
  i=$((i + 1))
  [ "$i" -lt 60 ] || { echo "sshd did not come up"; docker logs "$NAME"; exit 1; }
  sleep 1
done
ssh-keyscan -T 2 -p "$PORT" 127.0.0.1 2>/dev/null | grep ssh-ed25519 >"$KNOWN"
: >"$EMPTY_KNOWN"

mkdir -p "$HOME_DIR"
cat >"$CFG" <<CONF
Host *
  UserKnownHostsFile $KNOWN
  GlobalKnownHostsFile /dev/null
  IdentityFile $KEY
  IdentitiesOnly yes
  PubkeyAuthentication yes
CONF

probe() {
  # $1 user, then extra anvil args; stdin is not a tty, stderr is captured.
  user=$1
  shift
  HOME="$HOME_DIR" "$ANVIL" -vvv --ssh-config "$CFG" "$@" __probe \
    --host 127.0.0.1 --port "$PORT" --user "$user" </dev/null \
    >"$TMP/out" 2>"$TMP/err"
}

step "(a) key user: probe returns distro=arch within 10 s"
start=$(date +%s)
probe key
code=$?
elapsed=$(( $(date +%s) - start ))
if [ "$code" -eq 0 ] && grep -qx "$DISTRO" "$TMP/out" && [ "$elapsed" -le 10 ]; then
  pass "distro=arch in ${elapsed}s"
else
  fail "exit $code after ${elapsed}s"; cat "$TMP/out" "$TMP/err"
fi
if [ -n "$(find "$HOME_DIR/.ssh/anvil" -type s 2>/dev/null)" ]; then
  pass "control socket under the temp HOME"
else
  fail "no control socket in $HOME_DIR/.ssh/anvil"
fi

step "(b) second run reuses the master"
probe key --json
code=$?
accepted=$(docker logs "$NAME" 2>&1 | grep -c 'Accepted publickey for key')
if [ "$code" -eq 0 ] && grep -q '"distro":"arch' "$TMP/out" \
  && ! grep -q ' -M -N -f ' "$TMP/err" && [ "$accepted" -eq 1 ]; then
  pass "no master spawn, one publickey auth in the sshd log"
else
  fail "exit $code, $accepted auths"; cat "$TMP/out" "$TMP/err"
fi

step "(c) password user without a tty gets the D3 message, exit 1"
probe pw
code=$?
if [ "$code" -eq 1 ] && grep -q 'key auth failed; run interactively for password sign-in' "$TMP/err" \
  && [ ! -s "$TMP/out" ]; then
  pass "AuthDenied"
else
  fail "exit $code"; cat "$TMP/out" "$TMP/err"
fi

step "(d) unknown host key is refused with the ssh hint"
sed "s|UserKnownHostsFile .*|UserKnownHostsFile $EMPTY_KNOWN|" "$CFG" >"$CFG.unknown"
mv "$CFG.unknown" "$CFG"
rm -rf "$HOME_DIR/.ssh/anvil"
probe key
code=$?
if [ "$code" -eq 1 ] && grep -q 'host key unknown or changed; run `ssh <host>` once' "$TMP/err"; then
  pass "HostKey"
else
  fail "exit $code"; cat "$TMP/out" "$TMP/err"
fi

if [ "$fails" -eq 0 ]; then
  echo "e2e: all checks passed"
else
  echo "e2e: $fails check(s) failed"
  exit 1
fi
