#!/usr/bin/env bash
# shoji-cargo.sh <source-dir> <command...>
#
# Runs a command (cargo by default when the first word is a cargo
# subcommand) in the shoji-build container: openSUSE Tumbleweed with the
# Wayland, DRM, libinput, seat and GL headers the guest lacks. $HOME is
# mounted at the same path, so path dependencies anywhere under it resolve
# and target directories land where they would on the host. The cargo and
# rustup caches are shared by every source tree.
#
#   shoji-cargo.sh ~/ShojiWM build --profile release-fast -p shoji_wm
#   shoji-cargo.sh ~/ShojiWM test -p shojiwm_lib runtime_key_binding
#   shoji-cargo.sh ~/formalwm/wm build --release
#   shoji-cargo.sh ~/ShojiWM -- bash -c 'cargo fmt --check'
set -euo pipefail
if [ $# -lt 2 ]; then
  sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' >&2
  exit 2
fi
src=$(realpath "$1")
shift
case "$src" in
  "$HOME"/*) ;;
  *) echo "shoji-cargo: $src is outside $HOME, which is the only directory the container sees" >&2; exit 2 ;;
esac
if [ "$1" = -- ]; then
  shift
else
  set -- cargo "$@"
fi

if ! podman image exists shoji-build; then
  podman build -t shoji-build - <<'D'
FROM registry.opensuse.org/opensuse/tumbleweed
RUN zypper -n in --no-recommends rustup clang gcc gcc-c++ make cmake pkgconf git-core curl tar gzip \
  wayland-devel wayland-protocols-devel libxkbcommon-devel systemd-devel libinput-devel \
  libgbm-devel Mesa-libEGL-devel libglvnd-devel libpixman-1-0-devel seatd-devel libdrm-devel \
  liblcms2-devel libxcb-devel xcb-util-cursor-devel dbus-1-devel pipewire-devel
D
fi

mkdir -p "$HOME/shoji-cache/cargo" "$HOME/shoji-cache/rustup"
tty_flag=()
[ -t 0 ] && [ -t 1 ] && tty_flag=(-t)
exec podman run --rm -i ${tty_flag[@]+"${tty_flag[@]}"} --userns=keep-id --security-opt label=disable \
  -v "$HOME:$HOME" -e CARGO_HOME="$HOME/shoji-cache/cargo" -e RUSTUP_HOME="$HOME/shoji-cache/rustup" \
  -e HOME=/tmp -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" -w "$src" shoji-build \
  bash -c 'rustup toolchain list | grep -q stable || rustup default stable >/dev/null; rustup component list --installed | grep -q rustfmt || rustup component add rustfmt >/dev/null; exec "$@"' -- "$@"
