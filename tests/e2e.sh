#!/bin/sh
# End-to-end checks against an Arch sshd container, through the real
# commands: a temp XDG_CONFIG_HOME holds the inventory (the container as `arch`
# for the key user and `archpw` for the password user, `closed` on a closed
# port) and a config with
# no secret names, so the keychain is never touched.
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
  arm64 | aarch64) BASE=$ARCH_ARM64; PLATFORM=linux/arm64 ;;
  *) BASE=$ARCH_AMD64; PLATFORM=linux/amd64 ;;
esac
NAME="anvil-e2e-$$"
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
  docker rm -f "$NAME" >/dev/null 2>&1
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

step "build image and binary"
docker build -q --platform "$PLATFORM" --build-arg "BASE=$BASE" -t "$IMAGE" tests/e2e >/dev/null &
image=$!
cargo build --quiet || exit 1
wait "$image" || exit 1
ANVIL=$(pwd)/target/debug/anvil
FAKE_EDITOR=$(pwd)/tests/fixtures/fake-editor.sh

step "start sshd"
ssh-keygen -q -t ed25519 -N '' -f "$KEY" || exit 1
docker run -d --rm --platform "$PLATFORM" --name "$NAME" -p 127.0.0.1::22 \
  -e AUTHORIZED_KEY="$(cat "$KEY.pub")" "$IMAGE" >/dev/null || exit 1
PORT=$(docker port "$NAME" 22/tcp | head -n1 | sed 's/.*://')
[ -n "$PORT" ] || { echo "no mapped port"; exit 1; }
REMOTE_HOSTNAME=$(docker inspect -f '{{.Config.Hostname}}' "$NAME")
LOCAL_HOSTNAME=$(hostname)

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

# The environment of every anvil run: temp HOME (control dir) and config dir,
# the fake editor as $EDITOR, no $VISUAL to shadow it.
run_env() {
  env -u VISUAL HOME="$HOME_DIR" XDG_CONFIG_HOME="$XDG" EDITOR="$FAKE_EDITOR" "$@"
}

# anvil with stdin not a tty; stdout and stderr captured.
anvil() {
  run_env "$ANVIL" --ssh-config "$CFG" "$@" </dev/null >"$TMP/out" 2>"$TMP/err"
}

DISTRO_JSON='"distro":"arch(arm)?"'
DISTRO_LINE='^  distro +arch(arm)?$'

step "(a)+(g) doctor --json: arch and local connectable, whole run within 10 s"
anvil doctor --json --target local --target arch
code=$?
elapsed=$(($(date +%s) - started))
if [ "$code" -eq 0 ] && [ "$elapsed" -le 10 ] \
  && grep -Eq "\"target\":\"arch\",\"connectable\":true,\"facts\":\{[^}]*$DISTRO_JSON" "$TMP/out" \
  && grep -q '"target":"local","connectable":true,"facts":{[^}]*"distro":"macos"' "$TMP/out" \
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
# Never target `local` with this config: it would run brew on this host.
PKG_CONFIG="$TMP/packages.toml"
# inetutils is preinstalled in the image, so `--needed` keeps pacman offline;
# another package would need the network and a fresh sync db.
cat >"$PKG_CONFIG" <<CONF
[packages]
install = ["inetutils"]

[[steps]]
run = "hostname"
CONF
PKG_RUN='sudo -n pacman -S --noconfirm --needed inetutils'
anvil --config "$PKG_CONFIG" doctor --json --target arch
code=$?
if [ "$code" -eq 0 ] && grep -Eq '"target":"arch","connectable":true,"facts":\{[^}]*\},"manager":"pacman"' "$TMP/out"; then
  pass "doctor reports pacman"
else
  fail "doctor with [packages]: exit $code"
fi
anvil --config "$PKG_CONFIG" apply --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qxF "  \$ $PKG_RUN" "$TMP/out" && grep -qxF '  ✔ ok · 2 steps' "$TMP/out"; then
  pass "apply ran the package step, then hostname"
else
  fail "apply with [packages]: exit $code"
fi
anvil --config "$PKG_CONFIG" apply --json --target arch
code=$?
if [ "$code" -eq 0 ] && grep -qF "\"steps\":[{\"run\":\"$PKG_RUN\",\"code\":0," "$TMP/out"; then
  pass "apply --json step 1 is the package step"
else
  fail "apply --json with [packages]: exit $code"
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
# Never target `local` with these files: it would run brew on this host.
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
      --config "$SUDO_CONFIG" "$@" </dev/null >"$TMP/out" 2>"$TMP/err"
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
  if [ "$code" -eq 0 ] && grep -qxF "  \$ $PKG_RUN" "$TMP/out" && grep -qxF '  ✔ ok · 2 steps' "$TMP/out" \
    && ! grep -qF -e "$PASSWORD" -e '***' "$TMP/out" "$TMP/err"; then
    pass "apply typed the password at sudo's prompt, the package step and hostname ran"
  else
    fail "apply with the sudo secret: exit $code"
  fi
  sudo_anvil apply --json --target archpw
  code=$?
  if [ "$code" -eq 0 ] && grep -qF "\"steps\":[{\"run\":\"$PKG_RUN\",\"code\":0," "$TMP/out" \
    && ! grep -qF -e "$PASSWORD" -e '***' "$TMP/out" "$TMP/err"; then
    pass "apply --json ok, no password in it"
  else
    fail "apply --json with the sudo secret: exit $code"
  fi
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
  # The targets pane may shorten the name (`ar… pw@…`), so match on the user.
  run_env HOME="$TMP/home-tui" PTY_PASSWORD="$PASSWORD" python3 tests/fixtures/pty_tui.py --prompts 1 \
    --wait '\[x\] ar\S* pw@' --send d --wait 'connectable +yes' --wait 'distro +arch' --send q \
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

if [ "$fails" -eq 0 ]; then
  echo "e2e: all checks passed"
else
  echo "e2e: $fails check(s) failed"
  exit 1
fi
