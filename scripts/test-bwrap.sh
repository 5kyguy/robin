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
  --bind "$PWD" /src \
  --chdir /src \
  --setenv HOME /home \
  --setenv CARGO_HOME "${CARGO_HOME:-/usr/local/cargo}" \
  --setenv RUSTUP_HOME "${RUSTUP_HOME:-/usr/local/rustup}" \
  --setenv PATH "/usr/local/cargo/bin:/usr/bin:/bin" \
  --setenv DISPLAY "" \
  --setenv WAYLAND_DISPLAY "" \
  -- \
  cargo test --offline
