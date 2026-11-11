#!/usr/bin/env bash
# sync-to-rig.sh [host]
#
# Mirrors this checkout, .git included, onto the rig's ~/ShojiWM so an
# uncommitted change builds there. The checkout here stays the source of
# truth; the rig's copy is never edited in place. target/ and node_modules/
# stay on each side.
set -euo pipefail
repo=$(cd "$(dirname "$(realpath "$0")")/../.." && pwd)
host=${1:-fstest3@fstestmac}
rsync -a --delete --exclude /target --exclude node_modules --exclude /docs/build \
  "$repo/" "$host:ShojiWM/"
