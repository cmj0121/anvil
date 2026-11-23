#!/bin/sh
# anvil probe: prints facts as key=value lines, always exits 0 and ends
# with anvil-probe-end=1. The body is one group reading </dev/null so nothing
# can block on the transport's stdin, which is closed once this script has been
# sent; the shell reads the whole group from stdin before it runs any of it.
# brew counts when on PATH or at a standard Homebrew prefix, which a
# non-login ssh shell may leave off PATH. accounts names the account tools,
# shadow-utils before busybox, and accounts_bin the one directory that holds
# every tool of that family (on PATH, else /usr/sbin or /sbin); none without.
{
  printf '%s\n' "hostname=$(hostname 2>/dev/null)"
  printf '%s\n' "os=$(uname -s 2>/dev/null)"
  printf '%s\n' "kernel=$(uname -r 2>/dev/null)"
  printf '%s\n' "arch=$(uname -m 2>/dev/null)"
  if [ -r /etc/os-release ]; then
    (
      . /etc/os-release >/dev/null 2>&1
      printf 'distro=%s\ndistro_version=%s\n' "$ID" "$VERSION_ID"
    ) || printf 'distro=\ndistro_version=\n'
  elif command -v sw_vers >/dev/null 2>&1; then
    echo "distro=macos"
    printf '%s\n' "distro_version=$(sw_vers -productVersion 2>/dev/null)"
  else
    echo "distro=unknown"
    echo "distro_version="
  fi
  printf '%s\n' "user=$(id -un 2>/dev/null)"
  printf '%s\n' "shell=$SHELL"
  pkg=""
  if command -v brew >/dev/null 2>&1; then
    pkg="brew"
  else
    for b in /opt/homebrew/bin/brew /usr/local/bin/brew \
      /home/linuxbrew/.linuxbrew/bin/brew; do
      [ -x "$b" ] && pkg="brew" && break
    done
  fi
  for m in pacman apt dnf apk; do
    command -v "$m" >/dev/null 2>&1 && pkg="$pkg $m"
  done
  printf '%s\n' "pkg=${pkg# }"
  if ! command -v sudo >/dev/null 2>&1; then
    echo "sudo=none"
  elif sudo -n true >/dev/null 2>&1; then
    echo "sudo=nopasswd"
  else
    echo "sudo=binary"
  fi
  accounts=none
  accounts_bin=
  for family in "shadow useradd usermod userdel groupadd groupmod groupdel" \
    "busybox adduser deluser addgroup delgroup"; do
    set -- $family
    found=$(command -v "$2" 2>/dev/null)
    case $found in
      /*) dirs="${found%/*} /usr/sbin /sbin" ;;
      *) dirs="/usr/sbin /sbin" ;;
    esac
    for d in $dirs; do
      all=1
      for t in $family; do
        [ "$t" = "$1" ] || [ -x "$d/$t" ] || all=
      done
      if [ -n "$all" ]; then
        accounts=$1
        accounts_bin=$d
        break 2
      fi
    done
  done
  printf 'accounts=%s\naccounts_bin=%s\n' "$accounts" "$accounts_bin"
  echo "anvil-probe-end=1"
} </dev/null
exit 0
