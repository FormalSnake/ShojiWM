#!/usr/bin/env bash
# shoji-run.sh [<config>] -- <command...>
#
# Starts a ShojiWM compositor on this slot's own vkms card, runs <command>
# inside its session and returns the command's exit status. The compositor,
# its children and the command are torn down as cgroups afterwards.
#
#   <config>   a TypeScript config: a directory holding src/index.tsx, or the
#              entry file itself. Omitted: the stable default config, or no
#              --config at all when SHOJI_BIN is set.
#
# Environment:
#   SHOJI_BIN          compositor binary (default ~/shoji-stable/bin/shoji_wm).
#                      Any binary built on shojiwm_lib takes the same command
#                      line, e.g. SHOJI_BIN=~/formalwm/target/release/formalwm.
#   SHOJI_ARGS         extra compositor arguments, split on spaces.
#   SHOJI_RUNTIME      --runtime-dir for a TypeScript config (default
#                      ~/shoji-stable/runtime); a ShojiWM checkout works.
#   SHOJI_RUN_TIMEOUT  seconds the command may run (default 900).
#   SHOJI_LOCK_WAIT    seconds to wait for the rig lock (default 3600).
#   SHOJI_KEEP_RUNS    run directories kept under ~/shoji-runs (default 30).
#
# Inside the command: WAYLAND_DISPLAY, XDG_RUNTIME_DIR (private to the run),
# SHOJIWM_SOCKET (the control socket: one line, "quit", "reload" or
# "key <chord>"),
# SHOJI_IPC_SOCKET (where a TypeScript config's createIpcServer() listens by
# default), SHOJI_RUN_DIR and SHOJI_RUN_LOG (the compositor's log).
set -euo pipefail

usage() { awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" >&2; exit 2; }

config=""
if [ $# -gt 0 ] && [ "$1" != -- ]; then
  config=$1
  shift
fi
[ $# -gt 0 ] && [ "$1" = -- ] || usage
shift
[ $# -gt 0 ] || usage

stable=$HOME/shoji-stable
bin=${SHOJI_BIN:-$stable/bin/shoji_wm}
[ -x "$bin" ] || { echo "shoji-run: no executable compositor at $bin" >&2; exit 125; }
bin=$(realpath "$bin")
runtime=$(realpath "${SHOJI_RUNTIME:-$stable/runtime}")

config_args=()
if [ -n "$config" ]; then
  if [ -d "$config" ]; then config=$config/src/index.tsx; fi
  [ -f "$config" ] || { echo "shoji-run: no config entry at $config" >&2; exit 125; }
  config_args=(--config "$(realpath "$config")" --runtime-dir "$runtime")
elif [ -z "${SHOJI_BIN:-}" ]; then
  config_args=(--config "$stable/default-config/src/index.tsx" --runtime-dir "$runtime")
fi
# shellcheck disable=SC2206
extra_args=(${SHOJI_ARGS:-})

# One DRM master per card, and the card is shared by every agent on this slot.
exec 9>"$HOME/shoji-rig.lock"
if ! flock -n 9; then
  echo "shoji-run: waiting for the rig lock (another run holds the card)" >&2
  flock -w "${SHOJI_LOCK_WAIT:-3600}" 9 || { echo "shoji-run: gave up waiting for the rig lock" >&2; exit 125; }
fi

# vkms sits on the faux bus; the mac rig makes one card per slot user,
# /sys/devices/faux/vkms-<user>, and a slot drives only its own.
card=""
for sysfs in /sys/class/drm/card[0-9]*; do
  case "$(readlink -f "$sysfs/device")" in
    */vkms-"$(id -un)") card=/dev/dri/$(basename "$sysfs") ;;
  esac
done
[ -n "$card" ] || { echo "shoji-run: no vkms card for $(id -un)" >&2; exit 125; }
for fd in /proc/[0-9]*/fd/*; do
  if [ "$(readlink "$fd" 2>/dev/null)" = "$card" ]; then
    pid=${fd#/proc/}; pid=${pid%%/*}
    echo "shoji-run: $card is held by $pid ($(tr '\0' ' ' < "/proc/$pid/cmdline" | cut -c1-120)); run through shoji-rig on the macbook, which waits for FormalShell's slot lock" >&2
    exit 125
  fi
done

runs=$HOME/shoji-runs
run_dir=$runs/$(date +%Y%m%d-%H%M%S)-$$
mkdir -p "$run_dir/log"
# Kept short: a socket path has to fit in 108 bytes.
rt=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/shoji-run-$$
mkdir -m 700 "$rt"
host_runtime_dir=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}

unit=shoji-run-$$
cmd_unit=shoji-run-cmd-$$
comp_pid=""
cleanup() {
  local status=$?
  XDG_RUNTIME_DIR=$host_runtime_dir systemctl --user stop "$cmd_unit.scope" "$unit.scope" >/dev/null 2>&1 || true
  [ -n "$comp_pid" ] && wait "$comp_pid" 2>/dev/null || true
  rm -rf "$rt"
  ls -1dt "$runs"/*/ 2>/dev/null | tail -n +$((${SHOJI_KEEP_RUNS:-30} + 1)) | xargs -r rm -rf
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT TERM

unset WAYLAND_DISPLAY DISPLAY WAYLAND_SOCKET HYPRLAND_INSTANCE_SIGNATURE SHOJIWM_SOCKET
export XDG_RUNTIME_DIR=$rt
export DBUS_SESSION_BUS_ADDRESS=unix:path=$rt/bus
export XDG_SESSION_TYPE=wayland
export LD_LIBRARY_PATH=$stable/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}

# Without seatd (the mac rig runs none) libseat finds no seat for an ssh
# login; noop opens the card straight, which the video group allows.
XDG_RUNTIME_DIR=$host_runtime_dir systemd-run --user --scope --quiet --collect --unit="$unit" \
  -p TimeoutStopSec=5 -p RuntimeMaxSec=$((${SHOJI_RUN_TIMEOUT:-900} + 120)) -- \
  env XDG_RUNTIME_DIR="$rt" LIBSEAT_BACKEND=noop SHOJI_TTY_DRM_DEVICE="$card" \
    SHOJI_LOG_DIR="$run_dir/log" SHOJI_LOG_ROTATE=off \
  bash -c 'dbus-daemon --session --address="$DBUS_SESSION_BUS_ADDRESS" --nofork --nopidfile >"$1/dbus.log" 2>&1 &
    shift; exec "$@"' -- "$run_dir" "$bin" --tty ${config_args[@]+"${config_args[@]}"} ${extra_args[@]+"${extra_args[@]}"} \
  >"$run_dir/stdout.log" 2>&1 &
comp_pid=$!
echo "$bin ${config_args[*]} ${extra_args[*]}" > "$run_dir/command"

# Ready once a Wayland client gets a wl_display.sync answered: the initial
# config has run by the time the event loop serves clients.
roundtrip() {
  python3 -I - "$1" <<'PY'
import socket, struct, sys
s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
s.settimeout(2)
s.connect(sys.argv[1])
s.sendall(struct.pack("<III", 1, (12 << 16) | 0, 2))
buf = b""
while True:
    chunk = s.recv(4096)
    if not chunk:
        sys.exit(1)
    buf += chunk
    while len(buf) >= 8:
        obj, word = struct.unpack("<II", buf[:8])
        size = word >> 16
        if len(buf) < size:
            break
        if obj == 2 and (word & 0xFFFF) == 0:
            sys.exit(0)
        buf = buf[size:]
PY
}

display=""
for _ in $(seq 300); do
  if ! kill -0 "$comp_pid" 2>/dev/null; then
    echo "shoji-run: the compositor exited during startup; $run_dir/stdout.log and log/latest.log:" >&2
    tail -n 20 "$run_dir/stdout.log" "$run_dir/log/latest.log" >&2 2>/dev/null || true
    exit 125
  fi
  for sock in "$rt"/wayland-[0-9]*; do
    case "$sock" in *.lock) continue ;; esac
    [ -S "$sock" ] && roundtrip "$sock" 2>/dev/null && display=$(basename "$sock") && break 2
  done
  sleep 0.1
done
[ -n "$display" ] || { echo "shoji-run: no Wayland socket answered within 30 s; see $run_dir" >&2; exit 125; }

if grep -q 'failed to run initial config lifecycle' "$run_dir/log/latest.log" 2>/dev/null; then
  echo "shoji-run: the config failed to load (the session runs on defaults):" >&2
  grep 'failed to run initial config lifecycle' "$run_dir/log/latest.log" | head -n 3 >&2
fi

export WAYLAND_DISPLAY=$display
export SHOJIWM_SOCKET=$rt/shojiwm.$display.sock
export SHOJI_IPC_SOCKET=$rt/shojiwm-$display.sock
export SHOJI_RUN_DIR=$run_dir
export SHOJI_RUN_LOG=$run_dir/log/latest.log
# The compositor advertises dmabuf on the vkms card, which Mesa cannot open
# for a client; clients render on llvmpipe into wl_shm instead.
export LIBGL_ALWAYS_SOFTWARE=1
echo "shoji-run: $display on $card, run directory $run_dir" >&2

status=0
XDG_RUNTIME_DIR=$host_runtime_dir systemd-run --user --scope --quiet --collect --unit="$cmd_unit" \
  -p TimeoutStopSec=5 -- env XDG_RUNTIME_DIR="$rt" \
  timeout -k 5 "${SHOJI_RUN_TIMEOUT:-900}" "$@" || status=$?
[ "$status" = 124 ] && echo "shoji-run: the command hit SHOJI_RUN_TIMEOUT" >&2
if ! kill -0 "$comp_pid" 2>/dev/null; then
  echo "shoji-run: the compositor died while the command ran; see $run_dir" >&2
fi
exit "$status"
