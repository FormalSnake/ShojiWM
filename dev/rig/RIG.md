# ShojiWM on the mac rig (fstest3)

Slot fstest3 on fstestmac runs ShojiWM on its own vkms card
(`/sys/devices/faux/vkms-fstest3`, one 1024x768 output named `Virtual-4`),
over ssh, with no display attached. Rendering is Mesa's software GL on the
card's GBM device. The source of everything here is `dev/rig/` in
FormalSnake/ShojiWM, branch `formal`; `~/ShojiWM/dev/rig/install-stable.sh`
refreshes it.

## Run a session

```sh
~/shoji-run.sh [<config>] -- <command...>
```

It takes `~/shoji-rig.lock` for the whole run (one DRM master per card, and
three agents share this one), starts the compositor on the card, waits until
a Wayland client gets an answer, runs the command inside the session, tears
everything down and exits with the command's status. 125 means the session
never came up; the log tail is printed.

- TypeScript config: `<config>` is a directory holding `src/index.tsx`, or
  the entry file. Omitted: the stable default config
  (`~/shoji-stable/default-config`).
- Any other compositor binary built on `shojiwm_lib` (a Rust config such as
  `formalwm`): `SHOJI_BIN=/path/to/formalwm ~/shoji-run.sh -- <command>`.
  It gets `--tty` and no `--config`; pass its own flags in `SHOJI_ARGS`.
  It runs with `~/shoji-stable/lib` on `LD_LIBRARY_PATH` (see limits).
- `SHOJI_RUN_TIMEOUT` (default 900 s) bounds the command,
  `SHOJI_LOCK_WAIT` (default 3600 s) the wait for the lock.

Inside the command:

| Variable | What |
| --- | --- |
| `WAYLAND_DISPLAY`, `XDG_RUNTIME_DIR` | the session; the runtime dir is private to the run |
| `DBUS_SESSION_BUS_ADDRESS` | a private session bus |
| `SHOJIWM_SOCKET` | the core's control socket: one line per connection, `quit` or `reload`, answers `ok` |
| `SHOJI_IPC_SOCKET` | where a TypeScript config's `createIpcServer()` listens by default |
| `SHOJI_RUN_DIR` | `~/shoji-runs/<stamp>-<pid>/`: `log/latest.log`, `stdout.log`, `command`; put screenshots here |
| `SHOJI_RUN_LOG` | the compositor's log |
| `LIBGL_ALWAYS_SOFTWARE=1` | clients cannot open the vkms card for dmabuf; they render to wl_shm |

The last 30 run directories are kept.

## Screenshots

`grim "$SHOJI_RUN_DIR/shot.png"` (wlr-screencopy). `grim -g "x,y wxh"` for a
region.

## Input

- Text into the focused client: `wtype "text"`, `wtype -k Return`,
  `wtype -M ctrl c -m ctrl`. Smithay delivers virtual-keyboard keys straight
  to the focused client, so they never reach the compositor's key binds.
- Compositor key binds and the pointer: not reachable yet; patch P10 on
  `formal` adds them. This section changes when it lands.

## Talking to the session

```sh
~/shoji-ipc workspaces.get                 # request, prints the result
~/shoji-ipc some.method '{"id": 3}'        # with params
~/shoji-ipc state.get --listen 5           # print every broadcast for 5 s
printf 'reload\n' | socat - UNIX-CONNECT:"$SHOJIWM_SOCKET"
```

A one-shot `echo ... | socat` gets no answer: socat's write side closes and
the TypeScript IPC server drops the client before it replies. Keep it open
(`(printf '%s\n' "$req"; sleep 1) | socat - UNIX-CONNECT:"$SHOJI_IPC_SOCKET"`)
or use `shoji-ipc`.

## Build

`~/shoji-cargo.sh <source-dir> <cargo args...>` runs cargo in the
`shoji-build` container (Tumbleweed with the headers the guest lacks). `$HOME`
is mounted at the same path, so path dependencies anywhere under it resolve
and `target/` lands where it would natively; cargo and rustup caches are
shared in `~/shoji-cache`. The container has network for git dependencies.

```sh
~/shoji-cargo.sh ~/formalwm/wm build --release          # another tree
~/shoji-cargo.sh ~/ShojiWM test -p shojiwm_rs
~/shoji-cargo.sh ~/formalwm/wm -- bash -c 'cargo fmt --check && cargo clippy'
~/shoji-build.sh                                         # ~/ShojiWM, release-fast shoji_wm
```

`~/ShojiWM` is the core agent's working tree and changes branches; build
your own trees elsewhere under `~`. Two builds of one tree queue on cargo's
lock. Use `-p`, not `--workspace`, unless you want the portal too.

## The stable compositor

`~/shoji-stable/` holds `bin/shoji_wm`, `runtime/` (`packages/shoji_wm` and
`tools/`, passed as `--runtime-dir`), `default-config/`, `lib/` and `REV`
(the `formal` commit it was built from). It is replaced only from a build
checked on the card, and only between runs (the swap takes the rig lock).

## A TypeScript config outside the ShojiWM tree

A config directory is `src/index.tsx` plus whatever it imports. At run time
`shoji_wm`, `shoji_wm/ipc`, `shoji_wm/jsx-runtime`, `shoji_wm/types` and
`shoji_wm/default-composition` are rewritten to `--runtime-dir`'s copy in
every module, wherever the file sits, so no `node_modules` is needed to run.
Relative imports may leave the config's root, through a symlinked directory
(`src/shared -> ~/elsewhere`) or an absolute path; both were checked here.
Extensionless imports resolve to `.ts`, `.tsx`, `index.ts`, `index.tsx`. A
config must assign `COMPOSITOR.window.composition` (`defaultWindowComposition`
from `shoji_wm/default-composition` will do) or it falls back to defaults and
`shoji-run.sh` says so. Typecheck needs `node_modules/shoji_wm ->
~/shoji-stable/runtime/packages/shoji_wm` and a tsconfig like
`~/shoji-stable/default-config/tsconfig.json`; the guest has no node, so run
`tsc` on the macbook.

## Known limits

- One output, 1024x768 at 60 Hz, scale 1. No hotplug, no second monitor.
- No libinput devices: no real keyboard, touchpad, gestures, lid or switches.
- vkms has no async page flip (tearing), no VRR, no syncobj, no 10-bit
  formats; patches that need those are unverified here.
- No Xwayland on the guest, so no X11 clients.
- The guest lacks `libxcb-cursor0`, `libxcb-render-util0`, `libxcb-image0`
  and `libxcb-util1`; `~/shoji-stable/lib` carries copies from the build
  image. A binary of your own needs `LD_LIBRARY_PATH=~/shoji-stable/lib`
  outside `shoji-run.sh`.
- The default config starts helpers the guest lacks (cliphist, fcitx5); they
  fail quietly.
