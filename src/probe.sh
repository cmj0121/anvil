#!/bin/sh
# anvil probe (D4): prints facts as key=value lines, always exits 0 and ends
# with anvil-probe-end=1. Every command reads </dev/null so nothing can block
# on the transport's stdin, which is closed once this script has been sent.
printf '%s\n' "hostname=$(hostname </dev/null 2>/dev/null)" </dev/null
printf '%s\n' "os=$(uname -s </dev/null 2>/dev/null)" </dev/null
printf '%s\n' "kernel=$(uname -r </dev/null 2>/dev/null)" </dev/null
printf '%s\n' "arch=$(uname -m </dev/null 2>/dev/null)" </dev/null
if [ -r /etc/os-release ] </dev/null; then
  printf '%s\n' "distro=$(. /etc/os-release </dev/null 2>/dev/null; printf '%s\n' "$ID" </dev/null)" </dev/null
  printf '%s\n' "distro_version=$(. /etc/os-release </dev/null 2>/dev/null; printf '%s\n' "$VERSION_ID" </dev/null)" </dev/null
elif command -v sw_vers >/dev/null 2>&1 </dev/null; then
  echo "distro=macos" </dev/null
  printf '%s\n' "distro_version=$(sw_vers -productVersion </dev/null 2>/dev/null)" </dev/null
else
  echo "distro=unknown" </dev/null
  echo "distro_version=" </dev/null
fi
printf '%s\n' "user=$(id -un </dev/null 2>/dev/null)" </dev/null
printf '%s\n' "shell=$SHELL" </dev/null
pkg=""
for m in brew pacman apt dnf apk; do
  command -v "$m" >/dev/null 2>&1 </dev/null && pkg="$pkg $m"
done
printf '%s\n' "pkg=${pkg# }" </dev/null
if ! command -v sudo >/dev/null 2>&1 </dev/null; then
  echo "sudo=none" </dev/null
elif sudo -n true >/dev/null 2>&1 </dev/null; then
  echo "sudo=nopasswd" </dev/null
else
  echo "sudo=binary" </dev/null
fi
echo "anvil-probe-end=1" </dev/null
exit 0
