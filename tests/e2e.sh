#!/bin/sh
# End-to-end checks against an Arch sshd container, through the real
# commands: a temp XDG_CONFIG_HOME holds the inventory (the container as `arch`
# for the key user and `archpw` for the password user, `closed` on a closed
# port, `archsd` for a second container with systemd as PID 1) and a config
# with no secret names, so the keychain is never touched. A temp XDG_STATE_HOME
# holds last-apply.json, so no apply records into the real state directory.
# The built anvil-audit is pinned as a doctor plugin for the drift checks, and
# an alpine container, reached through a docker-exec stand-in for ssh, runs the
# hash run under busybox.
# The systemd container needs --privileged; when it cannot boot, the summary
# says `SKIP systemd: <reason>` and the run still passes, unless
# ANVIL_E2E_SYSTEMD=1, which turns that skip into a failure.
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
SD_IMAGE=anvil-e2e-systemd
# Base images pinned by manifest-list digest. To bump: `docker buildx imagetools
# inspect archlinux:latest` (and lopsided/archlinux:latest), copy the Digest line.
ARCH_AMD64=archlinux@sha256:52dfbd5afb23a220d5ee92dafcb4c788d96c026ed04179890c23adef4c897fa0
ARCH_ARM64=lopsided/archlinux@sha256:e895dfb834a8a60b42c2a43d6f447403b94ade083609ba0130032767d2bb34e1
# The official archlinux image is amd64 only and sshd's seccomp sandbox fails
# under emulation, so arm64 hosts build from an Arch Linux ARM image (ID=archarm).
case "$(uname -m)" in
  arm64 | aarch64) BASE=$ARCH_ARM64; PLATFORM=linux/arm64 ;;
  *) BASE=$ARCH_AMD64; PLATFORM=linux/amd64 ;;
esac
NAME="anvil-e2e-$$"
SD_NAME="$NAME-sd"
# The least-privileged set that boots systemd under Docker's cgroup v2: without
# --privileged, Docker mounts /sys/fs/cgroup read-only and PID 1 exits.
SD_FLAGS="--cgroupns=private --tmpfs /run --tmpfs /tmp --privileged"
# Seconds from `docker run` until systemd is up and its sshd answers.
SD_DEADLINE=60
# Why the systemd block was skipped; empty when it ran.
sd_skip=""
# The container's test-only password for the `pw` user (tests/e2e/Dockerfile).
PASSWORD=pw-secret
# Short: the control socket path must fit sun_path, and macOS' $TMPDIR is long.
TMP=$(mktemp -d /tmp/anvil-e2e.XXXXXX) || exit 1
HOME_DIR="$TMP/home"
XDG="$TMP/xdg"
CFG="$TMP/ssh_config"
KNOWN="$TMP/known_hosts"
EMPTY_KNOWN="$TMP/known_hosts.empty"
KEY="$TMP/id_ed25519"
fails=0

cleanup() {
  docker rm -f "$NAME" "$SD_NAME" "$NAME-alpine" >/dev/null 2>&1
  rm -rf "$TMP"
}
trap cleanup EXIT INT TERM

step() {
  printf '%s\n' "--- $*"
  started=$(date +%s)
}
took() { echo "$(($(date +%s) - started))s"; }
pass() { printf 'ok    %s (%s)\n' "$*" "$(took)"; }
fail() {
  printf 'FAIL  %s (%s)\n' "$*" "$(took)"
  fails=$((fails + 1))
  cat "$TMP/out" "$TMP/err" 2>/dev/null
}

# A run killed by SIGKILL skips its trap and leaves its containers behind.
# Each container names its run's shell in anvil-e2e.pid; only those whose run
# is gone are removed, so concurrent runs in other worktrees keep theirs.
for id in $(docker ps -aq --filter label=anvil-e2e=1); do
  pid=$(docker inspect -f '{{index .Config.Labels "anvil-e2e.pid"}}' "$id" 2>/dev/null)
  kill -0 "$pid" 2>/dev/null || docker rm -f "$id" >/dev/null 2>&1
done

step "build images and binary"
docker build -q --platform "$PLATFORM" --build-arg "BASE=$BASE" --target sshd -t "$IMAGE" tests/e2e >/dev/null &
image=$!
docker build -q --platform "$PLATFORM" --build-arg "BASE=$BASE" --target systemd -t "$SD_IMAGE" tests/e2e \
  >"$TMP/sd-build" 2>&1 &
sd_image=$!
cargo build --quiet || exit 1
cargo build --quiet -p anvil-audit || exit 1
wait "$image" || exit 1
wait "$sd_image" || { sd_skip="image build failed"; cat "$TMP/sd-build"; }
ANVIL=$(pwd)/target/debug/anvil
FAKE_EDITOR=$(pwd)/tests/fixtures/fake-editor.sh
# anvil-audit pinned as a doctor plugin, from a 0700 dir as a plugin must be.
mkdir -m 700 "$TMP/plugins"
install -m 0700 target/debug/anvil-audit "$TMP/plugins/audit"
AUDIT_SHA=$( (sha256sum "$TMP/plugins/audit" 2>/dev/null || shasum -a 256 "$TMP/plugins/audit") | cut -d' ' -f1)
AUDIT_PLUGIN=$(printf '[plugins.audit]\npath = "%s"\nsha256 = "%s"\non = "doctor"\n' "$TMP/plugins/audit" "$AUDIT_SHA")
# Busybox's sh, find and sha256sum for the hash run (block q): alpine:3.20, pinned
# by manifest-list digest like the Arch images above.
ALPINE=alpine@sha256:d9e853e87e55526f6b2917df91a2115c36dd7c696a35be12163d44e6e2a4b6bc

step "start sshd"
ssh-keygen -q -t ed25519 -N '' -f "$KEY" || exit 1
docker run -d --rm --platform "$PLATFORM" --name "$NAME" \
  --label anvil-e2e=1 --label "anvil-e2e.pid=$$" -p 127.0.0.1::22 \
  -e AUTHORIZED_KEY="$(cat "$KEY.pub")" "$IMAGE" >/dev/null || exit 1
PORT=$(docker port "$NAME" 22/tcp | head -n1 | sed 's/.*://')
[ -n "$PORT" ] || { echo "no mapped port"; exit 1; }
REMOTE_HOSTNAME=$(docker inspect -f '{{.Config.Hostname}}' "$NAME")
LOCAL_HOSTNAME=$(hostname)

# systemd boots while the sshd cases run; its block waits for it.
if [ -z "$sd_skip" ]; then
  # The host's kernel setting, read before the privileged boot from the
  # unprivileged sshd container, which cannot change it; block (n) checks it again.
  core_pattern=$(docker exec "$NAME" cat /proc/sys/kernel/core_pattern)
  sd_started=$(date +%s)
  # -t: systemd logs to the console, so `docker logs` holds the boot messages.
  # shellcheck disable=SC2086 # SD_FLAGS is a list of flags
  docker run -d -t --platform "$PLATFORM" --name "$SD_NAME" \
    --label anvil-e2e=1 --label "anvil-e2e.pid=$$" $SD_FLAGS -p 127.0.0.1::22 "$SD_IMAGE" \
    >"$TMP/sd-run" 2>&1 || { sd_skip="docker run failed: $(tail -n1 "$TMP/sd-run")"; }
fi

# The scan that finds sshd up is also the known_hosts entry.
i=0
until ssh-keyscan -T 2 -p "$PORT" 127.0.0.1 2>/dev/null | grep ssh-ed25519 >"$KNOWN"; do
  i=$((i + 1))
  [ "$i" -lt 60 ] || { echo "sshd did not come up"; docker logs "$NAME"; exit 1; }
  sleep 1
done
: >"$EMPTY_KNOWN"

mkdir -p "$HOME_DIR" "$XDG/anvil"
cat >"$CFG" <<CONF
Host *
  UserKnownHostsFile $KNOWN
  GlobalKnownHostsFile /dev/null
  IdentityFile $KEY
  IdentitiesOnly yes
  PubkeyAuthentication yes
CONF
cat >"$XDG/anvil/inventory.toml" <<INV
[hosts.arch]
host = "127.0.0.1"
user = "key"
port = $PORT

[hosts.archpw]
host = "127.0.0.1"
user = "pw"
port = $PORT

[hosts.closed]
host = "127.0.0.1"
port = 1
INV
cat >"$XDG/anvil/config.toml" <<CONF
[anvil]
format = "toml"

[[steps]]
run = "hostname"
CONF

# The environment of every anvil run: temp HOME (control dir), config dir and
# state dir (last-apply.json), the fake editor as $EDITOR, no $VISUAL to shadow it.
run_env() {
  env -u VISUAL HOME="$HOME_DIR" XDG_CONFIG_HOME="$XDG" XDG_STATE_HOME="$TMP/state" EDITOR="$FAKE_EDITOR" "$@"
}

# The permission bits of a local path in octal: GNU stat, else BSD stat.
local_mode() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }

# anvil with stdin not a tty; stdout and stderr captured.
anvil() {
  run_env "$ANVIL" --ssh-config "$CFG" "$@" </dev/null >"$TMP/out" 2>"$TMP/err"
}

DISTRO_JSON='"distro":"arch(arm)?"'
DISTRO_LINE='^  distro +arch(arm)?$'
# The local distro as the probe reports it: macos on Darwin, else os-release ID.
if [ "$(uname -s)" = Darwin ]; then
  LOCAL_DISTRO_JSON='"distro":"macos"'
elif [ -r /etc/os-release ]; then
  # shellcheck disable=SC1091 # the host's file, read at run time
  LOCAL_DISTRO_JSON="\"distro\":\"$(. /etc/os-release && printf '%s' "$ID")\""
else
  LOCAL_DISTRO_JSON=''
fi

step "(a)+(g) doctor --json: arch and local connectable, whole run within 10 s"
anvil doctor --json --target local --target arch
code=$?
elapsed=$(($(date +%s) - started))
if [ "$code" -eq 0 ] && [ "$elapsed" -le 10 ] \
  && grep -Eq "\"target\":\"arch\",\"connectable\":true,\"facts\":\{[^}]*$DISTRO_JSON" "$TMP/out" \
  && grep -Eq "\"target\":\"local\",\"connectable\":true,\"facts\":\{[^}]*$LOCAL_DISTRO_JSON" "$TMP/out" \
  && grep -q '"secrets":{}' "$TMP/out" && grep -q '"ok":true}$' "$TMP/out"; then
  pass "arch + local connectable, returned in ${elapsed}s"
else
  fail "exit $code after ${elapsed}s"
fi
if [ -n "$(find "$HOME_DIR/.ssh/anvil" -type s 2>/dev/null)" ]; then
  pass "control socket under the temp HOME"
else
  fail "no control socket in $HOME_DIR/.ssh/anvil"
fi

step "(b) a second doctor reuses the master"
anvil -vvv doctor --target local --target arch
code=$?
accepted=$(docker logs "$NAME" 2>&1 | grep -c 'Accepted publickey for key')
if [ "$code" -eq 0 ] && grep -Eq "$DISTRO_LINE" "$TMP/out" \
  && ! grep -q ' -M -N -f ' "$TMP/err" && [ "$accepted" -eq 1 ]; then
  pass "no master spawn, one publickey auth in the sshd log"
else
  fail "exit $code, $accepted auths"
fi

step "(c) apply prints each target's hostname"
anvil apply --target local --target arch
code=$?
expected=$(printf '▶ local\n  $ hostname\n  %s\n  ✔ ok · 1 step\n\n▶ arch\n  $ hostname\n  %s\n  ✔ ok · 1 step\n\n2 ok, 0 failed' \
  "$LOCAL_HOSTNAME" "$REMOTE_HOSTNAME")
if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ]; then
  pass "local $LOCAL_HOSTNAME, arch $REMOTE_HOSTNAME"
else
  fail "exit $code, expected:"
  printf '%s\n' "$expected"
fi
anvil apply --json --target local --target arch
code=$?
step_json() { printf '{"run":"hostname","code":0,"stdout":"%s\\n","stderr":""}' "$1"; }
expected=$(printf '{"targets":[{"target":"local","steps":[%s],"ok":true},{"target":"arch","steps":[%s],"ok":true}],"ok":true}' \
  "$(step_json "$LOCAL_HOSTNAME")" "$(step_json "$REMOTE_HOSTNAME")")
if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ]; then
  pass "apply --json shape"
else
  fail "exit $code, expected:"
  printf '%s\n' "$expected"
fi

step "(h) config --format=yaml edits in YAML, config.toml stays TOML, doctor still works"
anvil config --format=yaml --json
code=$?
if [ "$code" -eq 0 ] && grep -qx "{\"path\":\"$XDG/anvil/config.toml\",\"changed\":false}" "$TMP/out" \
  && [ ! -e "$XDG/anvil/config.yaml" ]; then
  printf '# edited as YAML\nanvil:\n  format: toml\nsteps:\n  - run: hostname\n' >"$TMP/config.yaml"
  run_env FAKE_EDITOR_CONTENT="$TMP/config.yaml" "$ANVIL" --ssh-config "$CFG" config --format=yaml --json \
    </dev/null >"$TMP/out" 2>"$TMP/err"
  code=$?
fi
if [ "$code" -eq 0 ] && grep -qx "{\"path\":\"$XDG/anvil/config.toml\",\"changed\":true}" "$TMP/out" \
  && [ ! -e "$XDG/anvil/config.yaml" ] && grep -qx 'format = "toml"' "$XDG/anvil/config.toml" \
  && grep -qx 'run = "hostname"' "$XDG/anvil/config.toml" && [ -f "$XDG/anvil/inventory.toml" ]; then
  anvil doctor --json --target local --target arch
  code=$?
  if [ "$code" -eq 0 ] && grep -q '"ok":true}$' "$TMP/out"; then
    pass "unchanged edit wrote nothing, YAML edit saved as config.toml, doctor ok"
  else
    fail "doctor after the YAML edit: exit $code"
  fi
else
  fail "config --format=yaml: exit $code"
  ls -l "$XDG/anvil"
fi

step "(d) password user without a tty gets the "run interactively" message, exit 1"
anvil doctor --target archpw
code=$?
if [ "$code" -eq 1 ] \
  && grep -q 'key auth failed; run interactively for password sign-in' "$TMP/out"; then
  pass "AuthDenied"
else
  fail "exit $code"
fi

step "(i) a host on a closed port is a failed target with an error, no steps"
anvil apply --json --target closed
code=$?
if [ "$code" -eq 1 ] \
  && grep -q '^{"targets":\[{"target":"closed","error":"cannot connect: [^"]*Connection refused","ok":false}\],"ok":false}$' "$TMP/out"; then
  pass "cannot connect, exit 1"
else
  fail "exit $code"
fi

step "(l) [packages] in a --config of its own: doctor resolves pacman, apply installs through sudo"
# Never target `local` with this config: it would run this host's package manager.
PKG_CONFIG="$TMP/packages.toml"
# inetutils is preinstalled in the image, so `--needed` keeps pacman offline;
# another package would need the network and a fresh sync db.
cat >"$PKG_CONFIG" <<CONF
[packages]
install = ["inetutils", "anvil-e2e-no-such-package"]

[[steps]]
run = "hostname"
CONF
# The package step's line ends in the install of the names pacman knows;
# the unknown name is skipped with a warning and the target stays ok.
PKG_TAIL='sudo -n pacman -S --noconfirm --needed $a; }'
PKG_WARN='  ⚠ packages: not found by pacman: anvil-e2e-no-such-package (add [packages.alias.NAME])'
anvil --config "$PKG_CONFIG" doctor --json --target arch
code=$?
if [ "$code" -eq 0 ] && grep -Eq '"target":"arch","connectable":true,"facts":\{[^}]*\},"manager":"pacman"' "$TMP/out"; then
  pass "doctor reports pacman"
else
  fail "doctor with [packages]: exit $code"
fi
anvil --config "$PKG_CONFIG" apply --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qF '  $ { anvil_db=1;' "$TMP/out" && grep -qF "$PKG_TAIL" "$TMP/out" \
  && grep -qxF "$PKG_WARN" "$TMP/out" && grep -qxF '  ✔ ok · 2 steps' "$TMP/out"; then
  pass "apply ran the package step, skipped the unknown name with a warning, then hostname"
else
  fail "apply with [packages]: exit $code"
fi
anvil --config "$PKG_CONFIG" apply --json --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qF "\"steps\":[{\"run\":\"{ anvil_db=1;" "$TMP/out" \
  && grep -qF "$PKG_TAIL\",\"code\":0," "$TMP/out" \
  && grep -qF '"warnings":["packages: not found by pacman: anvil-e2e-no-such-package (add [packages.alias.NAME])"]' "$TMP/out"; then
  pass "apply --json step 1 is the package step, the unknown name is a warning"
else
  fail "apply --json with [packages]: exit $code"
fi

step "(l) [[files]] on arch (NOPASSWD): sudo tee writes, refuses a symlinked dest or a user-owned directory, an unmatched entry is skipped"
# The probed distro: arch, or archarm on the arm64 image.
REMOTE_DISTRO=$(docker exec "$NAME" sh -c '. /etc/os-release && printf %s "$ID"')
FILES_DIR="$TMP/files"
mkdir -p "$FILES_DIR"
printf 'managed = "anvil-e2e"\n' >"$FILES_DIR/managed.conf"
# sudo = false writes as key, so it still follows the link in key's home.
cat >"$FILES_DIR/config.toml" <<CONF
[[files]]
src = "managed.conf"
dest = "/etc/anvil-e2e-plain"
mode = "0644"
facts = { distro = "$REMOTE_DISTRO" }

[[files]]
src = "managed.conf"
dest = "/etc/anvil-e2e-skip"
mode = "0644"
facts = { distro = "not-$REMOTE_DISTRO" }

[[files]]
src = "managed.conf"
dest = "/home/key/anvil-e2e-link"
mode = "0600"
sudo = false
CONF
cat >"$FILES_DIR/link.toml" <<CONF
[[files]]
src = "managed.conf"
dest = "/etc/anvil-e2e-link"
mode = "0644"
CONF
# Root writes nothing under key's home, which key could swap for a link.
cat >"$FILES_DIR/home.toml" <<CONF
[[files]]
src = "managed.conf"
dest = "/home/key/anvil-e2e-root"
mode = "0644"
CONF
docker exec "$NAME" sh -c 'printf old >/etc/anvil-e2e-real && chmod 600 /etc/anvil-e2e-real \
  && ln -s anvil-e2e-real /etc/anvil-e2e-link'
docker exec -u key "$NAME" sh -c 'printf old >/home/key/anvil-e2e-real && ln -s anvil-e2e-real /home/key/anvil-e2e-link'
anvil --config "$FILES_DIR/config.toml" apply --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  $ file /etc/anvil-e2e-plain' "$TMP/out" \
  && grep -qxF '  wrote /etc/anvil-e2e-plain' "$TMP/out" \
  && grep -qxF '  $ skipped file /etc/anvil-e2e-skip: no match' "$TMP/out" \
  && grep -qxF '  wrote /home/key/anvil-e2e-link' "$TMP/out" \
  && docker exec "$NAME" cat /etc/anvil-e2e-plain | cmp -s - "$FILES_DIR/managed.conf" \
  && [ "$(docker exec "$NAME" stat -c '%a %U:%G' /etc/anvil-e2e-plain)" = '644 root:root' ] \
  && ! docker exec "$NAME" test -e /etc/anvil-e2e-skip \
  && [ "$(docker exec "$NAME" readlink /home/key/anvil-e2e-link)" = anvil-e2e-real ] \
  && docker exec "$NAME" cat /home/key/anvil-e2e-real | cmp -s - "$FILES_DIR/managed.conf" \
  && ! grep -qF 'managed = ' "$TMP/out" "$TMP/err"; then
  pass "sudo tee wrote a root-owned file, sudo = false wrote through a link, the unmatched entry was skipped"
else
  fail "apply with [[files]]: exit $code"
fi
anvil --config "$FILES_DIR/config.toml" apply --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  unchanged /etc/anvil-e2e-plain' "$TMP/out" \
  && grep -qxF '  unchanged /home/key/anvil-e2e-link' "$TMP/out"; then
  pass "a rerun prints unchanged"
else
  fail "rerun with [[files]]: exit $code"
fi
anvil --config "$FILES_DIR/link.toml" apply --target arch
code=$?
if [ "$code" -eq 1 ] && grep -qxF '  $ file /etc/anvil-e2e-link' "$TMP/out" \
  && grep -qF 'files: /etc/anvil-e2e-link is a symlink; point dest at the real file' "$TMP/err" \
  && [ "$(docker exec "$NAME" readlink /etc/anvil-e2e-link)" = anvil-e2e-real ] \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-real)" = old ] \
  && [ "$(docker exec "$NAME" stat -c '%a %U:%G' /etc/anvil-e2e-real)" = '600 root:root' ]; then
  pass "sudo refused the symlinked dest, link and target untouched"
else
  fail "apply to a symlinked dest: exit $code"
fi
anvil --config "$FILES_DIR/home.toml" apply --target arch
code=$?
if [ "$code" -eq 1 ] && grep -qxF '  $ file /home/key/anvil-e2e-root' "$TMP/out" \
  && grep -qF 'files: /home/key is not root-only; root will not write under it' "$TMP/err" \
  && ! docker exec "$NAME" test -e /home/key/anvil-e2e-root; then
  pass "sudo refused a dest under key's home and wrote nothing"
else
  fail "apply to a dest under a user-owned directory: exit $code"
fi
# 1 MiB, the src limit: bash `sh -s` over ssh carries the text twice (D13).
yes 'anvil e2e: one line of a 1 MiB text file' | head -c 1048576 >"$FILES_DIR/big.txt"
cat >"$FILES_DIR/big.toml" <<CONF
[[files]]
src = "big.txt"
dest = "/etc/anvil-e2e-big"
mode = "0644"
CONF
step "(l) a 1 MiB file on arch within 10 s"
anvil --config "$FILES_DIR/big.toml" apply --target arch
code=$?
elapsed=$(($(date +%s) - started))
if [ "$code" -eq 0 ] && [ "$elapsed" -le 10 ] && grep -qxF '  wrote /etc/anvil-e2e-big' "$TMP/out" \
  && docker exec "$NAME" cat /etc/anvil-e2e-big | cmp -s - "$FILES_DIR/big.txt"; then
  pass "1 MiB written in ${elapsed}s"
else
  fail "1 MiB file: exit $code after ${elapsed}s"
fi

step "(o) rollback on arch (NOPASSWD): apply records, rollback restores bytes, mode and owner, removes a new dest"
# A state dir of its own, so the record holds only this block's apply.
RB_STATE="$TMP/state-rb"
RB_RECORD="$RB_STATE/anvil/last-apply.json"
RB_DIR="$TMP/rollback"
mkdir -p "$RB_DIR"
RB_OLD="anvil-e2e-rb-previous-$$"
RB_NEW="anvil-e2e-rb-written-$$"
printf 'new = "%s"\n' "$RB_NEW" >"$RB_DIR/new.conf"
cat >"$RB_DIR/config.toml" <<CONF
[[files]]
src = "new.conf"
dest = "/etc/anvil-e2e-rb"
mode = "0644"

[[files]]
src = "new.conf"
dest = "/etc/anvil-e2e-rb-new"
mode = "0644"
CONF
rb_anvil() {
  run_env XDG_STATE_HOME="$RB_STATE" "$ANVIL" --ssh-config "$CFG" --config "$RB_DIR/config.toml" "$@" \
    </dev/null >"$TMP/out" 2>"$TMP/err"
}
# Neither the previous nor the written bytes ever reach the output, logs included.
rb_quiet() { ! grep -qF -e "$RB_OLD" -e "$RB_NEW" "$TMP/out" "$TMP/err"; }
rb_stat() { docker exec "$NAME" stat -c '%a %U:%G' "$1"; }
# Previous: root:key 0600 (tee keeps the owner, so its mode is what changes).
docker exec "$NAME" sh -c "printf '%s\n' '$RB_OLD' >/etc/anvil-e2e-rb && chown root:key /etc/anvil-e2e-rb \
  && chmod 600 /etc/anvil-e2e-rb && rm -f /etc/anvil-e2e-rb-new"
rb_anvil -vv apply --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  wrote /etc/anvil-e2e-rb' "$TMP/out" \
  && grep -qxF '  wrote /etc/anvil-e2e-rb-new' "$TMP/out" \
  && docker exec "$NAME" cat /etc/anvil-e2e-rb | cmp -s - "$RB_DIR/new.conf" \
  && [ "$(rb_stat /etc/anvil-e2e-rb)" = '644 root:key' ] \
  && [ "$(rb_stat /etc/anvil-e2e-rb-new)" = '644 root:root' ] && rb_quiet; then
  pass "apply wrote a root-owned dest and a new one, no bytes in the output or the -vv log"
else
  fail "apply with [[files]] to record: exit $code"
fi
if [ "$(local_mode "$RB_STATE/anvil")" = 700 ] && [ "$(local_mode "$RB_RECORD")" = 600 ] \
  && grep -qF "$RB_OLD" "$RB_RECORD" && grep -qF '"previous":{"state":"absent"}' "$RB_RECORD"; then
  pass "last-apply.json is 0600 in the 0700 \$XDG_STATE_HOME/anvil, the previous text kept"
else
  fail "the record: dir $(local_mode "$RB_STATE/anvil"), file $(local_mode "$RB_RECORD")"
fi
rb_anvil apply --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  unchanged /etc/anvil-e2e-rb' "$TMP/out" && grep -qF "$RB_OLD" "$RB_RECORD"; then
  pass "a rerun apply is unchanged and keeps the previous text"
else
  fail "rerun apply: exit $code"
fi
rb_anvil rollback --dry-run --target arch
code=$?
# The config's default hostname step is step 1, which a rollback cannot undo.
expected=$(printf '▶ arch\n  would remove /etc/anvil-e2e-rb-new\n  would restore /etc/anvil-e2e-rb\n  cannot undo: step 1\n  ✔ 3 steps\n\n1 target, nothing executed')
if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ] \
  && docker exec "$NAME" cat /etc/anvil-e2e-rb | cmp -s - "$RB_DIR/new.conf"; then
  pass "rollback --dry-run lists the restore and the remove, changes nothing"
else
  fail "rollback --dry-run: exit $code, expected:"
  printf '%s\n' "$expected"
fi
rb_anvil -vv rollback --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  restored /etc/anvil-e2e-rb' "$TMP/out" \
  && grep -qxF '  removed /etc/anvil-e2e-rb-new' "$TMP/out" \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rb)" = "$RB_OLD" ] \
  && [ "$(rb_stat /etc/anvil-e2e-rb)" = '600 root:key' ] \
  && ! docker exec "$NAME" test -e /etc/anvil-e2e-rb-new && rb_quiet \
  && grep -qF '"rolled_back":{' "$RB_RECORD"; then
  pass "rollback restored the previous bytes, mode 0600 and owner root:key, removed the new dest"
else
  fail "rollback: exit $code"
  docker exec "$NAME" stat -c '%a %U:%G %n' /etc/anvil-e2e-rb
fi
rb_anvil rollback --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  unchanged /etc/anvil-e2e-rb' "$TMP/out" \
  && grep -qxF '  unchanged /etc/anvil-e2e-rb-new' "$TMP/out" \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rb)" = "$RB_OLD" ]; then
  pass "a second rollback is unchanged"
else
  fail "second rollback: exit $code"
fi
# Applied again, then edited by hand: the guard refuses until --force DEST.
rb_anvil apply --target arch
code=$?
docker exec "$NAME" sh -c 'printf "edited by hand\n" >/etc/anvil-e2e-rb'
rb_anvil rollback --target arch
code2=$?
if [ "$code" -eq 0 ] && [ "$code2" -eq 1 ] \
  && grep -qxF '  ✘ step 2 exited 3' "$TMP/out" \
  && grep -qxF '  arch: files: /etc/anvil-e2e-rb changed since the last apply; anvil rollback --force /etc/anvil-e2e-rb restores it anyway' "$TMP/err" \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rb)" = 'edited by hand' ]; then
  pass "a hand-edited dest is refused and left as edited"
else
  fail "rollback over a hand edit: apply exit $code, rollback exit $code2"
fi
rb_anvil rollback --force /etc/anvil-e2e-rb --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  restored /etc/anvil-e2e-rb' "$TMP/out" \
  && grep -qxF '  unchanged /etc/anvil-e2e-rb-new' "$TMP/out" \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rb)" = "$RB_OLD" ] \
  && [ "$(rb_stat /etc/anvil-e2e-rb)" = '600 root:key' ] \
  && ! docker exec "$NAME" test -e /etc/anvil-e2e-rb-new && rb_quiet; then
  pass "--force DEST restored the hand-edited dest"
else
  fail "rollback --force: exit $code"
fi
# A write killed midway: a tee that writes 4 bytes of the file, then exits 1.
# Its own -a (the 0600 create) goes to the real tee, set back right after.
docker exec -i "$NAME" sh -c 'mv /usr/bin/tee /usr/bin/tee.anvil-e2e && cat >/usr/bin/tee && chmod 755 /usr/bin/tee' <<'STUB'
#!/bin/sh
case $1 in -a) exec /usr/bin/tee.anvil-e2e "$@" ;; esac
head -c 4 >"$1"
exit 1
STUB
rb_anvil apply --target arch
code=$?
docker exec "$NAME" mv /usr/bin/tee.anvil-e2e /usr/bin/tee
if [ "$code" -eq 1 ] \
  && grep -qE '"dest":"/etc/anvil-e2e-rb","mode":"0644","sudo":true,"written_sha256":"[0-9a-f]{64}","write":"failed_midway"' "$RB_RECORD" \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rb)" = "$(head -c 4 "$RB_DIR/new.conf")" ]; then
  pass "a write that fails midway leaves part of the bytes, recorded as failed_midway"
else
  fail "apply with a tee that fails midway: exit $code"
fi
rb_anvil rollback --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF '  restored /etc/anvil-e2e-rb' "$TMP/out" \
  && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rb)" = "$RB_OLD" ] \
  && [ "$(rb_stat /etc/anvil-e2e-rb)" = '600 root:key' ] && rb_quiet; then
  pass "rollback restored the dest a write left half-written, without --force"
else
  fail "rollback after a write failed midway: exit $code"
fi

step "(p) drift on arch: anvil-audit pinned, a changed fact and file are drift, a re-apply clears it"
DR_STATE="$TMP/state-drift"
DR_DIR="$TMP/drift"
mkdir -p "$DR_DIR"
DR_TEXT="anvil-e2e-drift-$$"
printf 'drift = "%s"\n' "$DR_TEXT" >"$DR_DIR/drift.conf"
cat >"$DR_DIR/config.toml" <<CONF
$AUDIT_PLUGIN

[[files]]
src = "drift.conf"
dest = "/etc/anvil-e2e-drift"
mode = "0644"
CONF
dr_anvil() {
  run_env XDG_STATE_HOME="$DR_STATE" "$ANVIL" --ssh-config "$CFG" --config "$DR_DIR/config.toml" "$@" \
    </dev/null >"$TMP/out" 2>"$TMP/err"
}
# No byte of the file, and no hash or tag, reaches the output or the -vv log.
dr_quiet() { ! grep -qF "$DR_TEXT" "$TMP/out" "$TMP/err" && ! grep -qE '[0-9a-f]{64}' "$TMP/out" "$TMP/err"; }
docker exec "$NAME" rm -f /etc/anvil-e2e-drift /usr/local/bin/apk
dr_anvil apply --target arch
code=$?
dr_anvil -vv doctor --target arch
code2=$?
if [ "$code" -eq 0 ] && [ "$code2" -eq 0 ] && grep -qE '^  audit +no drift$' "$TMP/out" && dr_quiet; then
  pass "right after an apply: no drift"
else
  fail "apply then doctor: apply exit $code, doctor exit $code2"
fi
# A fact (pkg gains apk) and the file change on the target.
docker exec "$NAME" sh -c 'printf "#!/bin/sh\n" >/usr/local/bin/apk && chmod 755 /usr/local/bin/apk \
  && echo edited >>/etc/anvil-e2e-drift'
dr_anvil -vv doctor --target arch
code=$?
if [ "$code" -eq 1 ] && grep -qxF '  audit           drift: pkg was pacman, now pacman apk' "$TMP/out" \
  && grep -qxF '  audit           drift: /etc/anvil-e2e-drift was written, now changed' "$TMP/out" \
  && grep -qF 'doctor: drift since the last apply' "$TMP/err" && dr_quiet; then
  pass "a changed fact and a changed file are drift, exit 1"
else
  fail "doctor after the changes: exit $code"
fi
dr_anvil apply --target arch
code=$?
dr_anvil doctor --target arch
code2=$?
if [ "$code" -eq 0 ] && [ "$code2" -eq 0 ] && grep -qE '^  audit +no drift$' "$TMP/out" \
  && docker exec "$NAME" cat /etc/anvil-e2e-drift | cmp -s - "$DR_DIR/drift.conf"; then
  pass "a re-apply clears the drift"
else
  fail "re-apply then doctor: apply exit $code, doctor exit $code2"
fi
docker exec "$NAME" rm -f /etc/anvil-e2e-drift /usr/local/bin/apk

step "(q) alpine (busybox sh, find, sha256sum): the hash run reads root-only files, changes are drift"
ALP_NAME="$NAME-alpine"
ALP_DIR="$TMP/alpine"
mkdir -p "$ALP_DIR"
# No network and no capabilities: the hash run needs only sh, find and
# sha256sum, and apply only writes files root already owns.
if docker run -d --rm --platform "$PLATFORM" --name "$ALP_NAME" --label anvil-e2e=1 \
  --label "anvil-e2e.pid=$$" --network none --cap-drop ALL "$ALPINE" sleep 600 \
  >/dev/null 2>"$TMP/err"; then
  # An ssh for the container, given to the debug build's --ssh-program: every
  # control command and master succeeds, a client runs its script there as root.
  cat >"$ALP_DIR/ssh" <<SH
#!/bin/sh
for arg in "\$@"; do
  case "\$arg" in -O | -M) exit 0 ;; esac
done
exec docker exec -i "$ALP_NAME" sh -s
SH
  chmod 755 "$ALP_DIR/ssh"
  printf '[hosts.alpine]\nhost = "alpine.invalid"\n' >"$ALP_DIR/inventory.toml"
  printf 'alpine = "%s"\n' "$DR_TEXT" >"$ALP_DIR/a.conf"
  cat >"$ALP_DIR/config.toml" <<CONF
$AUDIT_PLUGIN

[[files]]
src = "a.conf"
dest = "/etc/anvil-e2e-alpine"
mode = "0644"

[[files]]
src = "a.conf"
dest = "/srv/anvil-e2e/b.conf"
mode = "0644"
CONF
  docker exec "$ALP_NAME" install -d -m 755 /srv/anvil-e2e
  alp_anvil() {
    run_env XDG_STATE_HOME="$TMP/state-alpine" "$ANVIL" --ssh-program "$ALP_DIR/ssh" \
      --inventory "$ALP_DIR/inventory.toml" --config "$ALP_DIR/config.toml" "$@" \
      </dev/null >"$TMP/out" 2>"$TMP/err"
  }
  alp_anvil apply --target alpine
  code=$?
  alp_anvil doctor --target alpine
  code2=$?
  if [ "$code" -eq 0 ] && [ "$code2" -eq 0 ] && grep -qE '^  audit +no drift$' "$TMP/out" \
    && grep -qE '^  distro +alpine$' "$TMP/out" && dr_quiet; then
    pass "busybox hashes both root-only files as written: no drift"
  else
    fail "apply then doctor on alpine: apply exit $code, doctor exit $code2"
  fi
  docker exec "$ALP_NAME" sh -c 'echo edited >>/etc/anvil-e2e-alpine && chmod 775 /srv/anvil-e2e'
  alp_anvil doctor --target alpine
  code=$?
  if [ "$code" -eq 1 ] && grep -qxF '  audit           drift: /etc/anvil-e2e-alpine was written, now changed' "$TMP/out" \
    && grep -qxF '  audit           drift: /srv/anvil-e2e/b.conf was written, now not root-only' "$TMP/out" \
    && dr_quiet; then
    pass "busybox: a changed file is changed, a group-writable parent is not root-only"
  else
    fail "doctor on alpine after the changes: exit $code"
  fi
  docker rm -f "$ALP_NAME" >/dev/null 2>&1
else
  fail "docker run $ALPINE"
fi

step "(e) password user through a pty: prompted once by doctor, not again by apply"
if command -v python3 >/dev/null 2>&1; then
  run_env PTY_PASSWORD="$PASSWORD" python3 tests/e2e/pty_login.py --prompts 1 --expect "$DISTRO_LINE" \
    -- "$ANVIL" --ssh-config "$CFG" doctor --target archpw >"$TMP/out" 2>"$TMP/err"
  code=$?
  if [ "$code" -eq 0 ] && docker logs "$NAME" 2>&1 | grep -q 'Accepted password for pw'; then
    pass "doctor got the facts after one password prompt"
  else
    fail "doctor over the pty: exit $code"
  fi
  run_env PTY_PASSWORD="$PASSWORD" python3 tests/e2e/pty_login.py --prompts 0 --expect "^  $REMOTE_HOSTNAME\$" \
    -- "$ANVIL" --ssh-config "$CFG" apply --target archpw >"$TMP/out" 2>"$TMP/err"
  code=$?
  if [ "$code" -eq 0 ]; then
    pass "apply reused the master, no prompt"
  else
    fail "apply over the pty: exit $code"
  fi
else
  echo "skip (no python3)"
fi

step "(m) sudo password from an age secret: archpw installs through sudo over a tty"
# Never target `local` with these files: it would run this host's package manager.
if command -v python3 >/dev/null 2>&1; then
  # A HOME of its own, so this check signs archpw in itself.
  SUDO_HOME="$TMP/home-sudo"
  SUDO_INV="$TMP/sudo-inventory.toml"
  SUDO_CONFIG="$TMP/sudo-config.toml"
  mkdir -p "$SUDO_HOME" "$TMP/age"
  # The fixture's age identity has no passphrase, so the store opens without a tty.
  cp tests/fixtures/sops/age-key.txt "$TMP/age-key.txt"
  chmod 600 "$TMP/age-key.txt"
  # archbad is archpw with a wrong password stored; both share the master (%C).
  cat >"$SUDO_INV" <<INV
[hosts.archpw]
host = "127.0.0.1"
user = "pw"
port = $PORT
sudo_secret = "PW_SUDO"

[hosts.archbad]
host = "127.0.0.1"
user = "pw"
port = $PORT
sudo_secret = "PW_BAD"
INV
  cat >"$SUDO_CONFIG" <<CONF
[secrets]
backend = "age"

[secrets.age]
path = "$TMP/age/secrets.age"
identity = "$TMP/age-key.txt"

[packages]
install = ["inetutils"]

[[steps]]
run = "hostname"
CONF
  # sudo_anvil [VAR=value...] ARGS...: anvil on these files, stdin not a tty.
  sudo_anvil() {
    sudo_env=""
    while [ $# -gt 0 ]; do
      case $1 in
        *=*) sudo_env="$sudo_env $1"; shift ;;
        *) break ;;
      esac
    done
    # shellcheck disable=SC2086 # the assignments hold no spaces
    run_env HOME="$SUDO_HOME" $sudo_env "$ANVIL" --ssh-config "$CFG" --inventory "$SUDO_INV" \
      --config "${SUDO_CONFIG_USED:-$SUDO_CONFIG}" "$@" </dev/null >"$TMP/out" 2>"$TMP/err"
  }
  # The password reaches the store only through the editor buffer.
  printf '%s\n' "$PASSWORD" >"$TMP/typed.good"
  printf 'not-the-password\n' >"$TMP/typed.bad"
  # The store is ASCII-armored age: the armor line first, never the password.
  sudo_anvil FAKE_EDITOR_CONTENT="$TMP/typed.good" secret PW_SUDO \
    && grep -qx 'secret PW_SUDO: updated' "$TMP/out" && ! grep -qF "$PASSWORD" "$TMP/out" "$TMP/err" \
    && sudo_anvil FAKE_EDITOR_CONTENT="$TMP/typed.bad" secret PW_BAD \
    && grep -qx 'secret PW_BAD: updated' "$TMP/out" && ! grep -qF "$PASSWORD" "$TMP/out" "$TMP/err" \
    && [ "$(head -n1 "$TMP/age/secrets.age")" = '-----BEGIN AGE ENCRYPTED FILE-----' ] \
    && ! grep -qF -e "$PASSWORD" -e 'not-the-password' "$TMP/age/secrets.age"
  code=$?
  if [ "$code" -eq 0 ]; then
    pass "PW_SUDO and PW_BAD stored in the age file, encrypted"
  else
    fail "secret into the age store: exit $code"
  fi
  run_env HOME="$SUDO_HOME" PTY_PASSWORD="$PASSWORD" python3 tests/e2e/pty_login.py --prompts 1 \
    --expect '^  secret PW_SUDO +ok$' \
    -- "$ANVIL" --ssh-config "$CFG" --inventory "$SUDO_INV" --config "$SUDO_CONFIG" \
    doctor --target archpw >"$TMP/out" 2>"$TMP/err"
  code=$?
  # sudo `binary` (not `nopasswd`): pw's sudo needs the password.
  if [ "$code" -eq 0 ] && grep -Eq '^  manager +pacman$' "$TMP/out" && grep -Eq '^  sudo +binary$' "$TMP/out" \
    && ! grep -qF "$PASSWORD" "$TMP/out" "$TMP/err"; then
    pass "doctor signed in by password, secret PW_SUDO ok, sudo binary, manager pacman"
  else
    fail "doctor over the pty: exit $code"
  fi
  sudo_anvil apply --target archpw
  code=$?
  if [ "$code" -eq 0 ] && grep -qF "$PKG_TAIL" "$TMP/out" && grep -qxF '  ✔ ok · 2 steps' "$TMP/out" \
    && ! grep -qF -e "$PASSWORD" -e '***' "$TMP/out" "$TMP/err"; then
    pass "apply typed the password at sudo's prompt, the package step and hostname ran"
  else
    fail "apply with the sudo secret: exit $code"
  fi
  sudo_anvil apply --json --target archpw
  code=$?
  if [ "$code" -eq 0 ] && grep -qF "\"steps\":[{\"run\":\"{ anvil_db=1;" "$TMP/out" \
    && grep -qF "$PKG_TAIL\",\"code\":0," "$TMP/out" \
    && ! grep -qF -e "$PASSWORD" -e '***' "$TMP/out" "$TMP/err"; then
    pass "apply --json ok, no password in it"
  else
    fail "apply --json with the sudo secret: exit $code"
  fi
  # [[files]] over the password path: staged in /tmp, installed by sudo over a tty.
  SUDO_FILES_DIR="$TMP/sudo-files"
  mkdir -p "$SUDO_FILES_DIR"
  FILE_CONTENT="anvil-e2e-content-$$"
  printf '# managed by the anvil e2e\nkey = "%s"\n' "$FILE_CONTENT" >"$SUDO_FILES_DIR/e2e.conf"
  cat >"$SUDO_FILES_DIR/config.toml" <<CONF
[secrets]
backend = "age"

[secrets.age]
path = "$TMP/age/secrets.age"
identity = "$TMP/age-key.txt"

[[files]]
src = "e2e.conf"
dest = "/etc/anvil-e2e.conf"
mode = "0640"
facts = { distro = "$REMOTE_DISTRO" }
CONF
  SUDO_CONFIG_USED="$SUDO_FILES_DIR/config.toml"
  sudo_anvil apply --target archpw
  code=$?
  if [ "$code" -eq 0 ] && grep -qxF '  $ file /etc/anvil-e2e.conf' "$TMP/out" \
    && grep -qxF '  wrote /etc/anvil-e2e.conf' "$TMP/out" \
    && docker exec "$NAME" cat /etc/anvil-e2e.conf | cmp -s - "$SUDO_FILES_DIR/e2e.conf" \
    && [ "$(docker exec "$NAME" stat -c '%a %U:%G' /etc/anvil-e2e.conf)" = '640 root:root' ] \
    && ! grep -qF -e "$PASSWORD" -e "$FILE_CONTENT" "$TMP/out" "$TMP/err"; then
    pass "the file went in through sudo with the password: bytes, mode 0640, root-owned"
  else
    fail "apply [[files]] with the sudo secret: exit $code"
    docker exec "$NAME" stat -c '%a %U:%G' /etc/anvil-e2e.conf
  fi
  sudo_anvil apply --json --target archpw
  code=$?
  left=$(docker exec "$NAME" sh -c 'ls -d /tmp/anvil-* 2>/dev/null')
  if [ "$code" -eq 0 ] \
    && grep -qF '{"run":"file /etc/anvil-e2e.conf","code":0,"stdout":"unchanged /etc/anvil-e2e.conf\n"' "$TMP/out" \
    && [ -z "$left" ] && ! grep -qF -e "$PASSWORD" -e "$FILE_CONTENT" "$TMP/out" "$TMP/err"; then
    pass "a rerun prints unchanged, no /tmp/anvil-* left on the target"
  else
    fail "rerun [[files]] with the sudo secret: exit $code, left: $left"
  fi
  # Rollback over the password path: staged in /tmp, put back by sudo install
  # with the previous owner and mode, from a state dir of its own.
  PW_STATE="$TMP/state-pw"
  PW_RECORD="$PW_STATE/anvil/last-apply.json"
  PW_OLD="anvil-e2e-pw-previous-$$"
  cat >>"$SUDO_FILES_DIR/config.toml" <<CONF

[[files]]
src = "e2e.conf"
dest = "/etc/anvil-e2e-rbpw"
mode = "0600"

[[files]]
src = "e2e.conf"
dest = "/etc/anvil-e2e-rbpw-new"
mode = "0600"
CONF
  docker exec "$NAME" sh -c "printf '%s\n' '$PW_OLD' >/etc/anvil-e2e-rbpw && chown pw:pw /etc/anvil-e2e-rbpw \
    && chmod 640 /etc/anvil-e2e-rbpw && rm -f /etc/anvil-e2e-rbpw-new"
  pw_quiet() { ! grep -qF -e "$PASSWORD" -e "$FILE_CONTENT" -e "$PW_OLD" "$TMP/out" "$TMP/err"; }
  sudo_anvil XDG_STATE_HOME="$PW_STATE" -vv apply --target archpw
  code=$?
  if [ "$code" -eq 0 ] && grep -qxF '  wrote /etc/anvil-e2e-rbpw' "$TMP/out" \
    && grep -qxF '  wrote /etc/anvil-e2e-rbpw-new' "$TMP/out" \
    && [ "$(docker exec "$NAME" stat -c '%a %U:%G' /etc/anvil-e2e-rbpw)" = '600 root:root' ] \
    && [ "$(local_mode "$PW_STATE/anvil")" = 700 ] && [ "$(local_mode "$PW_RECORD")" = 600 ] \
    && ! grep -qF "$PASSWORD" "$PW_RECORD" && pw_quiet; then
    pass "apply over the password path took a pw-owned dest to root:root 0600, recorded at 0600"
  else
    fail "apply [[files]] to record with the sudo secret: exit $code"
  fi
  sudo_anvil XDG_STATE_HOME="$PW_STATE" -vv rollback --target archpw
  code=$?
  left=$(docker exec "$NAME" sh -c 'ls -d /tmp/anvil-* 2>/dev/null')
  if [ "$code" -eq 0 ] && grep -qxF '  restored /etc/anvil-e2e-rbpw' "$TMP/out" \
    && grep -qxF '  removed /etc/anvil-e2e-rbpw-new' "$TMP/out" \
    && [ "$(docker exec "$NAME" cat /etc/anvil-e2e-rbpw)" = "$PW_OLD" ] \
    && [ "$(docker exec "$NAME" stat -c '%a %U:%G' /etc/anvil-e2e-rbpw)" = '640 pw:pw' ] \
    && ! docker exec "$NAME" test -e /etc/anvil-e2e-rbpw-new && [ -z "$left" ] && pw_quiet; then
    pass "rollback with the sudo secret restored the bytes, mode 0640 and owner pw:pw, removed the new dest"
  else
    fail "rollback with the sudo secret: exit $code, left: $left"
    docker exec "$NAME" stat -c '%a %U:%G %n' /etc/anvil-e2e-rbpw
  fi
  # Doctor never types the sudo password: archpw's root-only file is counted,
  # not compared, with one warning.
  printf '%s\n' "$AUDIT_PLUGIN" >"$SUDO_FILES_DIR/doctor.toml"
  SUDO_CONFIG_USED="$SUDO_FILES_DIR/doctor.toml"
  sudo_anvil doctor --target archpw
  code=$?
  if [ "$code" -eq 0 ] && grep -qxF '  files           1 not compared (1 no sudo)' "$TMP/out" \
    && [ "$(grep -cE '^  sudo +WARNING: ' "$TMP/out")" = 1 ] && grep -qE '^  audit +no drift$' "$TMP/out" \
    && ! grep -qF -e "$PASSWORD" -e "$FILE_CONTENT" "$TMP/out" "$TMP/err"; then
    pass "doctor on archpw: the root-only file is no_sudo, one warning, exit 0"
  else
    fail "doctor with anvil-audit on archpw: exit $code"
  fi
  SUDO_CONFIG_USED=$SUDO_CONFIG
  # One wrong attempt only, and last: pam_faillock locks pw after three.
  sudo_anvil apply --json --target archbad
  code=$?
  if [ "$code" -eq 1 ] && grep -qF 'packages: sudo rejected the password from secret PW_BAD' "$TMP/out" \
    && grep -q '"ok":false}$' "$TMP/out" \
    && ! grep -qF -e "$PASSWORD" -e 'not-the-password' "$TMP/out" "$TMP/err"; then
    pass "a wrong stored password fails the target, naming the secret"
  else
    fail "apply with a wrong sudo secret: exit $code"
  fi
else
  echo "skip (no python3)"
fi

step "(k) anvil tui: doctor on the password user, ssh's prompt handed over from the worker"
# pty_tui.py needs os.login_tty: python3 3.11 or newer.
if python3 -c 'import os; os.login_tty' 2>/dev/null; then
  # A HOME of its own, so the master (e) left behind does not answer for archpw.
  mkdir -p "$TMP/home-tui"
  before=$(docker logs "$NAME" 2>&1 | grep -c 'Accepted password for pw')
  run_env HOME="$TMP/home-tui" PTY_PASSWORD="$PASSWORD" python3 tests/fixtures/pty_tui.py --prompts 1 \
    --wait 'targets: archpw' --send d --wait 'connectable +yes' --wait 'distro +arch' --send q \
    -- "$ANVIL" --ssh-config "$CFG" tui --target archpw >"$TMP/out" 2>"$TMP/err"
  code=$?
  after=$(docker logs "$NAME" 2>&1 | grep -c 'Accepted password for pw')
  if [ "$code" -eq 0 ] && [ "$after" -eq $((before + 1)) ]; then
    pass "password typed at ssh's prompt, archpw connectable, terminal restored"
  else
    fail "tui doctor over the pty: exit $code, $((after - before)) password sign-ins"
  fi
else
  echo "skip (no python3 with os.login_tty, 3.11 or newer)"
fi

step "(n) systemd as PID 1: boots within ${SD_DEADLINE} s, anvil run reaches archsd, [[services]] starts, enables, stops and disables a unit"
# SD_DEADLINE s after docker run, and never less than 30 s from here.
sd_deadline=$((${sd_started:-0} + SD_DEADLINE))
[ "$sd_deadline" -ge $(($(date +%s) + 30)) ] || sd_deadline=$(($(date +%s) + 30))
# Polls `is-system-running` without --wait, which would block past the deadline.
sd_wait() {
  while :; do
    sd_state=$(docker exec "$SD_NAME" systemctl is-system-running 2>/dev/null)
    case $sd_state in running | degraded) return 0 ;; esac
    if [ "$(docker inspect -f '{{.State.Running}}' "$SD_NAME" 2>/dev/null)" != true ]; then
      docker logs "$SD_NAME" 2>&1 | tr -d '\033\r' | sed 's/\[[0-9;?]*[A-Za-z]//g' >"$TMP/sd-log"
      sd_state=$(grep -a -m1 -oE '(Failed|failed|Error|error).*' "$TMP/sd-log") \
        || sd_state=$(tail -n1 "$TMP/sd-log")
      sd_state="exited: $sd_state"
      return 1
    fi
    [ "$(date +%s)" -lt "$sd_deadline" ] || return 1
    sleep 1
  done
}
if [ -z "$sd_skip" ] && ! sd_wait; then
  sd_skip="systemd did not boot (${sd_state:-no state}; flags: $SD_FLAGS)"
fi
if [ -z "$sd_skip" ]; then
  docker exec -i "$SD_NAME" sh -c 'install -d -m 700 -o key -g key /home/key/.ssh \
    && cat > /home/key/.ssh/authorized_keys && chown key:key /home/key/.ssh/authorized_keys \
    && chmod 600 /home/key/.ssh/authorized_keys' <"$KEY.pub"
  SD_PORT=$(docker port "$SD_NAME" 22/tcp | head -n1 | sed 's/.*://')
  until ssh-keyscan -T 2 -p "$SD_PORT" 127.0.0.1 2>/dev/null | grep ssh-ed25519 >>"$KNOWN"; do
    [ "$(date +%s)" -lt "$sd_deadline" ] \
      || { sd_skip="sshd did not answer by the deadline (systemd $sd_state)"; break; }
    sleep 1
  done
fi
if [ -z "$sd_skip" ]; then
  printf 'systemd %s, %s, flags: %s\n' "$sd_state" \
    "$(docker exec "$SD_NAME" systemd-analyze 2>/dev/null | head -n1)" "$SD_FLAGS"
  cat >>"$XDG/anvil/inventory.toml" <<INV

[hosts.archsd]
host = "127.0.0.1"
user = "key"
port = $SD_PORT
tags = ["sd"]
INV
  SD_HOSTNAME=$(docker inspect -f '{{.Config.Hostname}}' "$SD_NAME")
  printf 'hostname\n' >"$TMP/hostname.sh"
  anvil run --target sd "$TMP/hostname.sh"
  code=$?
  if [ "$code" -eq 0 ] && grep -qxF "  $SD_HOSTNAME" "$TMP/out" && grep -qxF '1 ok, 0 failed' "$TMP/out" \
    && [ "$(docker exec "$SD_NAME" cat /proc/1/comm)" = systemd ]; then
    pass "PID 1 is systemd, anvil run printed $SD_HOSTNAME"
  else
    fail "anvil run on archsd: exit $code"
  fi
  after=$(docker exec "$NAME" cat /proc/sys/kernel/core_pattern)
  if [ -n "$core_pattern" ] && [ "$after" = "$core_pattern" ]; then
    pass "the host's kernel.core_pattern is unchanged by the boot"
  else
    fail "kernel.core_pattern was '$core_pattern', now '$after'"
  fi

  # [[services]] on archsd: key's sudo is NOPASSWD, so each command goes through `sudo -n`.
  SVC_DIR="$TMP/services"
  mkdir -p "$SVC_DIR"
  svc_config() { # FILE STATE ENABLED
    printf '[[services]]\nname = "anvil-e2e"\nstate = "%s"\nenabled = %s\ntags = ["sd"]\n' "$2" "$3" >"$SVC_DIR/$1"
  }
  svc_config start.toml started true
  svc_config stop.toml stopped false
  cat >"$SVC_DIR/missing.toml" <<CONF
[[services]]
name = "anvil-e2e-missing"
state = "started"
tags = ["sd"]

[[steps]]
run = "hostname"
CONF
  # Exact stdout, the default hostname step included: the service step prints
  # anvil's lines, never systemctl's chatter (that goes to stderr).
  svc_out() {
    printf '▶ archsd\n  $ service anvil-e2e\n'
    printf '  %s\n' "$@"
    printf '  $ hostname\n  %s\n  ✔ ok · 2 steps\n\n1 ok, 0 failed' "$SD_HOSTNAME"
  }
  svc_unit() { # ACTIVE ENABLED
    [ "$(docker exec "$SD_NAME" systemctl is-active anvil-e2e)" = "$1" ] \
      && [ "$(docker exec "$SD_NAME" systemctl is-enabled anvil-e2e)" = "$2" ]
  }
  # sudo logs each command it runs to the journal; sync it before counting.
  svc_sudo() {
    docker exec "$SD_NAME" sh -c 'journalctl --sync; journalctl -q --no-pager _COMM=sudo' | grep -cF "COMMAND=/usr/bin/systemctl --no-ask-password $1 -- anvil-e2e"
  }

  anvil --config "$SVC_DIR/start.toml" apply --target sd
  code=$?
  expected=$(svc_out 'enabled service anvil-e2e' 'started service anvil-e2e')
  if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ] && svc_unit active enabled \
    && [ "$(svc_sudo 'enable --now')" -eq 1 ]; then
    pass "started + enabled: enable --now through sudo -n, unit active and enabled"
  else
    fail "apply started + enabled: exit $code, expected:"
    printf '%s\n' "$expected"
    docker exec "$SD_NAME" systemctl is-active anvil-e2e
    docker exec "$SD_NAME" systemctl is-enabled anvil-e2e
  fi
  anvil --config "$SVC_DIR/start.toml" apply --target sd
  code=$?
  expected=$(svc_out 'unchanged service anvil-e2e')
  if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ] && svc_unit active enabled \
    && [ "$(svc_sudo 'enable --now')" -eq 1 ]; then
    pass "a rerun prints unchanged and changes nothing"
  else
    fail "rerun started + enabled: exit $code, expected:"
    printf '%s\n' "$expected"
  fi
  anvil --config "$SVC_DIR/stop.toml" apply --target sd
  code=$?
  expected=$(svc_out 'disabled service anvil-e2e' 'stopped service anvil-e2e')
  if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ] && svc_unit inactive disabled \
    && [ "$(svc_sudo 'disable --now')" -eq 1 ]; then
    pass "stopped + disabled: disable --now through sudo -n, unit inactive and disabled"
  else
    fail "apply stopped + disabled: exit $code, expected:"
    printf '%s\n' "$expected"
  fi
  anvil --config "$SVC_DIR/stop.toml" apply --target sd
  code=$?
  expected=$(svc_out 'unchanged service anvil-e2e')
  if [ "$code" -eq 0 ] && [ "$(cat "$TMP/out")" = "$expected" ] && svc_unit inactive disabled \
    && [ "$(svc_sudo 'disable --now')" -eq 1 ]; then
    pass "a rerun prints unchanged and changes nothing"
  else
    fail "rerun stopped + disabled: exit $code, expected:"
    printf '%s\n' "$expected"
  fi
  anvil --config "$SVC_DIR/missing.toml" apply --target archsd --target arch
  code=$?
  if [ "$code" -ne 0 ] && grep -qF 'services: anvil-e2e-missing: not found by systemd' "$TMP/out" "$TMP/err" \
    && grep -qxF '  $ skipped service anvil-e2e-missing: no match' "$TMP/out" \
    && grep -qxF "  $REMOTE_HOSTNAME" "$TMP/out" && grep -qxF '1 ok, 1 failed' "$TMP/out"; then
    pass "an unknown unit fails archsd; arch skips it (no match) and runs hostname"
  else
    fail "apply anvil-e2e-missing: exit $code"
  fi
else
  printf 'SKIP  systemd: %s (%s)\n' "$sd_skip" "$(took)"
fi

step "(f) unknown host key is refused with the ssh hint"
sed "s|UserKnownHostsFile .*|UserKnownHostsFile $EMPTY_KNOWN|" "$CFG" >"$CFG.unknown"
mv "$CFG.unknown" "$CFG"
rm -rf "$HOME_DIR/.ssh/anvil"
anvil doctor --target arch
code=$?
if [ "$code" -eq 1 ] \
  && grep -q 'host key unknown or changed; run `ssh <host>` once' "$TMP/out"; then
  pass "HostKey"
else
  fail "exit $code"
fi

step "(j) a secret round trip through a sops store (anvil and sops run on this host)"
if command -v sops >/dev/null 2>&1; then
  SOPS_XDG="$TMP/xdg-sops"
  SOPS_STORE="$TMP/sops/secrets.json"
  mkdir -p "$SOPS_XDG/anvil" "$TMP/sops"
  cp tests/fixtures/sops/.sops.yaml "$TMP/sops/"
  cp tests/fixtures/sops/age-key.txt "$TMP/sops-key.txt"
  chmod 600 "$TMP/sops-key.txt"
  cat >"$SOPS_XDG/anvil/config.toml" <<CONF
[secrets]
backend = "sops"
names = ["HOST_SUDO"]

[secrets.sops]
path = "$SOPS_STORE"
age_key_file = "$TMP/sops-key.txt"
CONF
  printf 'e2e-s3cr3t\n' >"$TMP/typed"
  run_env XDG_CONFIG_HOME="$SOPS_XDG" FAKE_EDITOR_CONTENT="$TMP/typed" "$ANVIL" secret HOST_SUDO \
    </dev/null >"$TMP/out" 2>"$TMP/err"
  code=$?
  if [ "$code" -eq 0 ] && grep -qx 'secret HOST_SUDO: updated' "$TMP/out" \
    && grep -q 'ENC\[AES256_GCM' "$SOPS_STORE" && ! grep -q 'e2e-s3cr3t' "$SOPS_STORE"; then
    run_env XDG_CONFIG_HOME="$SOPS_XDG" "$ANVIL" doctor --json --target local \
      </dev/null >"$TMP/out" 2>"$TMP/err"
    code=$?
    if [ "$code" -eq 0 ] && grep -q '"secrets":{"HOST_SUDO":"ok"}' "$TMP/out" \
      && ! grep -q 'e2e-s3cr3t' "$TMP/out" "$TMP/err"; then
      pass "stored encrypted, doctor finds it"
    else
      fail "doctor on the sops store: exit $code"
    fi
  else
    fail "secret into the sops store: exit $code"
  fi
else
  rm -f "$TMP/out" "$TMP/err"
  fail "sops not found on PATH: install sops (e.g. \`brew install sops\`) to run this check"
fi

if [ -n "$sd_skip" ]; then
  echo "SKIP systemd: $sd_skip"
  if [ "${ANVIL_E2E_SYSTEMD:-}" = 1 ]; then
    echo "ANVIL_E2E_SYSTEMD=1: the systemd skip counts as a failure"
    fails=$((fails + 1))
  fi
fi
if [ "$fails" -eq 0 ]; then
  echo "e2e: all checks passed"
else
  echo "e2e: $fails check(s) failed"
  exit 1
fi
