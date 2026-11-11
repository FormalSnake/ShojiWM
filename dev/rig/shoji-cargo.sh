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
#   CARGO_TARGET_DIR=~/shoji-pr/target shoji-cargo.sh ~/shoji-pr/src/<branch> test -p shojiwm_rs
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

# The image is rebuilt when this recipe changes. Fonts: the text layout
# tests panic without a default font. /tmp/.X11-unix: the Xwayland tests
# want it owned by root, as CI prepares it.
recipe='FROM registry.opensuse.org/opensuse/tumbleweed
RUN zypper -n in --no-recommends rustup clang gcc gcc-c++ make cmake pkgconf git-core curl tar gzip \
  wayland-devel wayland-protocols-devel libxkbcommon-devel systemd-devel libinput-devel \
  libgbm-devel Mesa-libEGL-devel libglvnd-devel libpixman-1-0-devel seatd-devel libdrm-devel \
  liblcms2-devel libxcb-devel xcb-util-cursor-devel dbus-1-devel pipewire-devel \
  fontconfig google-noto-sans-fonts dejavu-fonts
RUN install -d -o root -g root -m 1777 /tmp/.X11-unix'
recipe_id=$(printf '%s' "$recipe" | sha256sum | cut -c1-16)
if [ "$(podman image inspect -f '{{index .Labels "shoji.recipe"}}' shoji-build 2>/dev/null)" != "$recipe_id" ]; then
  printf '%s\nLABEL shoji.recipe=%s\n' "$recipe" "$recipe_id" | podman build -t shoji-build -
fi

# The guest's / is shared by every slot's agents; a build that fills it kills
# everyone's links mid-way.
free_gb=$(df --output=avail -BG "$HOME" | tail -1 | tr -dc 0-9)
if [ "$free_gb" -lt 15 ]; then
  echo "shoji-cargo: only ${free_gb}G free on $(df --output=target "$HOME" | tail -1); clean your own target/ dirs first (see ~/RIG.md, Disk)" >&2
  exit 3
fi

mkdir -p "$HOME/shoji-cache/cargo" "$HOME/shoji-cache/rustup"
tty_flag=()
[ -t 0 ] && [ -t 1 ] && tty_flag=(-t)
exec podman run --rm -i ${tty_flag[@]+"${tty_flag[@]}"} --userns=keep-id --security-opt label=disable \
  -v "$HOME:$HOME" -e CARGO_HOME="$HOME/shoji-cache/cargo" -e RUSTUP_HOME="$HOME/shoji-cache/rustup" \
  -e HOME=/tmp -e CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-auto}" \
  ${CARGO_TARGET_DIR:+-e CARGO_TARGET_DIR="$CARGO_TARGET_DIR"} -w "$src" shoji-build \
  bash -c 'rustup toolchain list | grep -q stable || rustup default stable >/dev/null; rustup component list --installed | grep -q rustfmt || rustup component add rustfmt >/dev/null; exec "$@"' -- "$@"
