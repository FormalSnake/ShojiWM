#!/usr/bin/env bash
# shoji-build.sh [cargo build args...]
#
# Builds ~/ShojiWM's shoji_wm with the release-fast profile in the
# shoji-build container, and records the commit it was built from next to
# the binary (install-stable.sh checks it). Any other tree:
# shoji-cargo.sh <dir> build ...
set -euo pipefail
src=${SHOJI_SRC:-$HOME/ShojiWM}
"$(dirname "$(realpath "$0")")/shoji-cargo.sh" "$src" build --profile release-fast -p shoji_wm "$@"
dirty=""
[ -n "$(git -C "$src" status --porcelain --untracked-files=no -- src Cargo.toml Cargo.lock)" ] && dirty=" dirty"
echo "$(git -C "$src" rev-parse HEAD)$dirty" > "$src/target/release-fast/shoji_wm.rev"
