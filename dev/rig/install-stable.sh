#!/usr/bin/env bash
# install-stable.sh
#
# Refreshes ~/shoji-stable on the rig from this checkout's release-fast
# build, and the rig scripts and RIG.md in ~ from dev/rig. Run it only after
# the build has been checked on the card. Waits for the rig lock, so no run
# sees a half-copied tree.
#
#   ~/shoji-stable/bin/shoji_wm        the compositor
#   ~/shoji-stable/lib/                xcb libraries the guest lacks
#   ~/shoji-stable/runtime/            packages/shoji_wm and tools/ (--runtime-dir)
#   ~/shoji-stable/default-config/     packages/config, typechecks against runtime/
#   ~/shoji-stable/REV                 the commit it was built from
set -euo pipefail
repo=$(cd "$(dirname "$(realpath "$0")")/../.." && pwd)
bin=$repo/target/release-fast/shoji_wm
[ -x "$bin" ] || { echo "install-stable: no build at $bin" >&2; exit 1; }
if [ -n "$(git -C "$repo" status --porcelain --untracked-files=no)" ]; then
  echo "install-stable: $repo has uncommitted changes; stable builds come from a commit" >&2
  exit 1
fi
if [ "$(stat -c %Y "$bin")" -lt "$(git -C "$repo" log -1 --format=%ct)" ]; then
  echo "install-stable: $bin is older than HEAD; rebuild first" >&2
  exit 1
fi

new=$HOME/shoji-stable.new
rm -rf "$new"
mkdir -p "$new/bin" "$new/lib" "$new/runtime/packages" "$new/runtime/tools"
cp "$bin" "$new/bin/shoji_wm"
cp -a "$repo/packages/shoji_wm" "$new/runtime/packages/"
cp "$repo/tools/decoration-runtime.ts" "$new/runtime/tools/"
cp -a "$repo/packages/config" "$new/default-config"
mkdir -p "$new/default-config/node_modules"
ln -s ../../runtime/packages/shoji_wm "$new/default-config/node_modules/shoji_wm"
cat > "$new/default-config/tsconfig.json" <<'J'
{
  "compilerOptions": {
    "target": "ES2022",
    "module": "ESNext",
    "moduleResolution": "Bundler",
    "jsx": "react-jsx",
    "jsxImportSource": "shoji_wm",
    "strict": true,
    "verbatimModuleSyntax": true,
    "noEmit": true,
    "types": []
  },
  "include": ["src/**/*.ts", "src/**/*.tsx"]
}
J
# The guest has no libxcb-cursor0, libxcb-render-util0, libxcb-image0 or
# libxcb-util1; the build image has them.
podman run --rm --security-opt label=disable -v "$new/lib:/out" shoji-build \
  cp -L /usr/lib64/libxcb-cursor.so.0 /usr/lib64/libxcb-render-util.so.0 \
  /usr/lib64/libxcb-image.so.0 /usr/lib64/libxcb-util.so.1 /out/
if LD_LIBRARY_PATH=$new/lib ldd "$new/bin/shoji_wm" | grep 'not found'; then
  echo "install-stable: the binary still misses libraries" >&2
  exit 1
fi
printf '%s %s\n' "$(git -C "$repo" rev-parse HEAD)" "$(date -Is)" > "$new/REV"

exec 9>"$HOME/shoji-rig.lock"
flock 9
rm -rf "$HOME/shoji-stable.old"
[ -d "$HOME/shoji-stable" ] && mv "$HOME/shoji-stable" "$HOME/shoji-stable.old"
mv "$new" "$HOME/shoji-stable"
rm -rf "$HOME/shoji-stable.old"
for f in shoji-run.sh shoji-cargo.sh shoji-build.sh shoji-ipc; do
  install -m 755 "$repo/dev/rig/$f" "$HOME/$f"
done
install -m 644 "$repo/dev/rig/RIG.md" "$HOME/RIG.md"
echo "install-stable: $(cat "$HOME/shoji-stable/REV")"
