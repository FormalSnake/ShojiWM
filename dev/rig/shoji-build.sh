#!/usr/bin/env bash
# shoji-build.sh [cargo build args...]
#
# Builds ~/ShojiWM's shoji_wm with the release-fast profile in the
# shoji-build container. Any other tree: shoji-cargo.sh <dir> build ...
exec "$(dirname "$(realpath "$0")")/shoji-cargo.sh" "${SHOJI_SRC:-$HOME/ShojiWM}" build --profile release-fast -p shoji_wm "$@"
