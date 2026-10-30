#!/bin/sh
# Run the suite with a private home so a bad path cannot touch the real one.
# DISPLAY is cleared: the render smoke test needs a display and runs outside
# this wrapper, under xvfb.
set -eu
cd "$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)"

if ! command -v bwrap >/dev/null 2>&1; then
  echo "bubblewrap is not installed" >&2
  exit 1
fi

binds=""
for dir in /usr /lib /lib64 /bin /etc; do
  if [ -d "$dir" ]; then
    binds="$binds --ro-bind $dir $dir"
  fi
done
if [ -d /usr/local ]; then
  binds="$binds --ro-bind /usr/local /usr/local"
fi

# GitHub's runner keeps rustup under $HOME, which the private /home tmpfs
# would hide. Bind the real cargo and rustup trees and put cargo's directory
# on PATH. Skip paths already covered by the /usr bind.
host_cargo="${CARGO_HOME:-${HOME:-/tmp}/.cargo}"
host_rustup="${RUSTUP_HOME:-${HOME:-/tmp}/.rustup}"
cargo_bin=$(command -v cargo)
cargo_dir=$(CDPATH= cd -- "$(dirname "$cargo_bin")" && pwd)
# Applied after --tmpfs /home so a private home does not cover rustup.
late_binds=""
add_ro() {
  dir=$1
  if [ ! -d "$dir" ]; then
    return 0
  fi
  case "$dir" in
    /usr | /usr/* | /lib | /lib/* | /lib64 | /lib64/* | /bin | /bin/* | /etc | /etc/*) return 0 ;;
  esac
  late_binds="$late_binds --ro-bind $dir $dir"
}
add_ro "$host_cargo"
add_ro "$host_rustup"

# shellcheck disable=SC2086
exec bwrap \
  --unshare-user \
  --unshare-pid \
  --unshare-ipc \
  --unshare-uts \
  --die-with-parent \
  $binds \
  --proc /proc \
  --dev /dev \
  --tmpfs /tmp \
  --tmpfs /home \
  $late_binds \
  --bind "$PWD" /src \
  --chdir /src \
  --setenv HOME /home \
  --setenv CARGO_HOME "$host_cargo" \
  --setenv RUSTUP_HOME "$host_rustup" \
  --setenv PATH "$cargo_dir:/usr/bin:/bin" \
  --setenv DISPLAY "" \
  --setenv WAYLAND_DISPLAY "" \
  -- \
  cargo test --offline
